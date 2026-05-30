use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::{ExecutorError, RuntimeError},
    provider::{HealthStatus, RuntimeProvider},
    registry::RuntimeRegistry,
    runtime_listener::{AcceptedConn, RuntimeListenerServer},
    socket_transport::SocketRuntimeTransport,
};
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, CommandFailedEvent, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand,
    ExecutorEvent, ExecutorInboundMessage, ExecutorOutboundMessage, RegisteredEvent,
    RestartRuntimeCmd, RuntimeConfig, RuntimeState, RuntimeStateChangedEvent, RuntimesListedEvent,
    ToolCallCmd, ToolResultEvent,
};
use models::runtime::{RuntimeOutboundMessage, ToolError, ToolResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
            Message,
        >,
    >,
>;

async fn send_outbound(sink: &WsSink, msg: ExecutorOutboundMessage) -> Result<(), ExecutorError> {
    let json =
        serde_json::to_string(&msg).map_err(|e| ExecutorError::Serialization(e.to_string()))?;
    sink.lock()
        .await
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| ExecutorError::SendFailed(e.to_string()))
}

async fn emit_state(sink: &WsSink, request_id: &str, runtime_id: &str, state: RuntimeState) {
    let _ = send_outbound(
        sink,
        ExecutorOutboundMessage {
            request_id: request_id.to_string(),
            event: ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                runtime_id: runtime_id.to_string(),
                state,
            }),
        },
    )
    .await;
}

/// Core runtime-creation transition, shared by the server WS path ([`do_create`])
/// and the in-process [`InMemExecutorTransport`](crate::InMemExecutorTransport).
/// Spawns the runtime (via the provider) and records it Running, or marks it Failed.
pub(crate) async fn create_core(
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    id: &str,
    config: RuntimeConfig,
) -> Result<(), RuntimeError> {
    registry.begin_create(id, config.clone()).await?;
    match provider.create(id, &config).await {
        Ok(handle) => {
            registry.complete_create(id, handle).await?;
            Ok(())
        }
        Err(e) => {
            let _ = registry.mark_failed(id).await;
            Err(e)
        }
    }
}

/// Accept runtime connections on `listener` and register each as a direct transport,
/// until `cancel` fires. Used by CLI mode (which drives lifecycle via
/// [`InMemExecutorTransport`](crate::InMemExecutorTransport)) to run the listener loop.
pub fn serve_runtime_connections(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(AcceptedConn::Tcp(ws)) => {
                        tokio::spawn(handle_runtime_connection(ws, registry.clone()));
                    }
                    Ok(AcceptedConn::Unix(ws)) => {
                        tokio::spawn(handle_runtime_connection(ws, registry.clone()));
                    }
                    Err(_) => break,
                }
            }
        }
        // Dropping `listener` here unlinks the unix socket (its Drop impl).
    });
}

pub struct Executor {
    executor_id: String,
    server_url: String,
    provider: Box<dyn RuntimeProvider>,
    health_check_interval: Duration,
    max_restarts: u32,
    runtime_listener: Option<RuntimeListenerServer>,
    connected_registry: Option<Arc<ConnectedRuntimeRegistry>>,
}

impl Executor {
    pub fn new(
        executor_id: String,
        server_url: String,
        provider: Box<dyn RuntimeProvider>,
    ) -> Self {
        Self {
            executor_id,
            server_url,
            provider,
            health_check_interval: Duration::from_secs(30),
            max_restarts: 3,
            runtime_listener: None,
            connected_registry: None,
        }
    }

    pub fn with_health_check_interval(mut self, interval: Duration) -> Self {
        self.health_check_interval = interval;
        self
    }

    pub fn with_max_restarts(mut self, max: u32) -> Self {
        self.max_restarts = max;
        self
    }

    pub fn with_runtime_listener(
        mut self,
        listener: RuntimeListenerServer,
        registry: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        self.runtime_listener = Some(listener);
        self.connected_registry = Some(registry);
        self
    }

    pub async fn run(self, cancel: CancellationToken) -> Result<(), ExecutorError> {
        let (ws, _) = connect_async(&self.server_url)
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))?;
        let (sink_inner, mut stream) = ws.split();
        let sink: WsSink = Arc::new(Mutex::new(sink_inner));

        send_outbound(
            &sink,
            ExecutorOutboundMessage {
                request_id: Uuid::new_v4().to_string(),
                event: ExecutorEvent::Registered(RegisteredEvent {
                    executor_id: self.executor_id.clone(),
                }),
            },
        )
        .await?;

        let registry = Arc::new(RuntimeRegistry::new());
        let provider: Arc<dyn RuntimeProvider> = Arc::from(self.provider);
        let max_restarts = self.max_restarts;
        let connected_registry = self.connected_registry;

        // Start the runtime listener if configured. The handler registers a direct
        // transport per connection; tool calls then flow through that transport.
        if let (Some(listener), Some(conn_reg)) =
            (self.runtime_listener, connected_registry.clone())
        {
            serve_runtime_connections(listener, conn_reg, cancel.clone());
        }

        let hc_sink = sink.clone();
        let hc_reg = registry.clone();
        let hc_prov = provider.clone();
        let hc_cancel = cancel.clone();
        let hc_interval = self.health_check_interval;
        let health_task = tokio::spawn(async move {
            let start = tokio::time::Instant::now() + hc_interval;
            let mut ticker = tokio::time::interval_at(start, hc_interval);
            loop {
                tokio::select! {
                    _ = hc_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        run_health_check(&hc_reg, &hc_prov, &hc_sink, max_restarts).await;
                    }
                }
            }
        });

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(inbound) = serde_json::from_str::<ExecutorInboundMessage>(&text) {
                                dispatch(&inbound, &registry, &provider, &sink, connected_registry.as_ref()).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(Message::Binary(_)))
                        | Some(Ok(Message::Ping(_)))
                        | Some(Ok(Message::Pong(_)))
                        | Some(Ok(Message::Frame(_))) => {}
                    }
                }
            }
        }

        health_task.abort();
        Ok(())
    }
}

/// Handshake on an accepted runtime link, then register it as a direct transport.
/// Generic over the socket type so TCP and unix share one accept/handshake/frame path.
async fn handle_runtime_connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    registry: Arc<ConnectedRuntimeRegistry>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut stream) = ws.split();

    // First message must be RuntimeReady, within a bounded handshake window so a
    // peer that connects but never announces itself can't leak this task forever.
    let handshake = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(RuntimeOutboundMessage::Ready(ev)) =
                        serde_json::from_str::<RuntimeOutboundMessage>(&text)
                    {
                        return Some(ev.runtime_id);
                    }
                }
                _ => return None,
            }
        }
    })
    .await;
    let runtime_id = match handshake {
        Ok(Some(id)) => id,
        // Timed out, stream closed, or non-Text/garbage before Ready — drop the link.
        Ok(None) | Err(_) => return,
    };

    let (transport, closed) = SocketRuntimeTransport::from_split(sink, stream);
    registry
        .register_transport(runtime_id.clone(), Arc::new(transport))
        .await;
    // Deregister when the link drops so health checks observe the loss and a stale
    // transport never lingers (explicit destroy also removes it; double-remove is safe).
    let _ = closed.await;
    registry.remove(&runtime_id).await;
}

async fn dispatch(
    msg: &ExecutorInboundMessage,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
) {
    let req = &msg.request_id;
    let result = match &msg.command {
        ExecutorCommand::CreateRuntime(cmd) => do_create(cmd, registry, provider, sink, req).await,
        ExecutorCommand::DestroyRuntime(cmd) => do_destroy(cmd, registry, sink, req).await,
        ExecutorCommand::RestartRuntime(cmd) => {
            do_restart(cmd, registry, provider, sink, req).await
        }
        ExecutorCommand::QueryRuntimes(_) => {
            let runtimes = registry.list().await;
            let _ = send_outbound(
                sink,
                ExecutorOutboundMessage {
                    request_id: req.clone(),
                    event: ExecutorEvent::RuntimesListed(RuntimesListedEvent { runtimes }),
                },
            )
            .await;
            Ok(())
        }
        ExecutorCommand::ToolCall(cmd) => do_tool_call(cmd, connected_registry, sink).await,
        ExecutorCommand::CancelToolCall(cmd) => do_cancel_tool_call(cmd, connected_registry).await,
    };
    if let Err(e) = result {
        let _ = send_outbound(
            sink,
            ExecutorOutboundMessage {
                request_id: req.clone(),
                event: ExecutorEvent::CommandFailed(CommandFailedEvent {
                    message: e.to_string(),
                }),
            },
        )
        .await;
    }
}

/// Server-mode tool relay: look up the runtime's direct transport, invoke the tool
/// on a spawned task (so the dispatch loop is not blocked), and forward the result
/// back to the server over the executor WS.
async fn do_tool_call(
    cmd: &ToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
    sink: &WsSink,
) -> Result<(), RuntimeError> {
    let reg = connected_registry
        .ok_or_else(|| RuntimeError::Provider("no runtime listener configured".to_string()))?;
    let transport = reg
        .runtime_transport(&cmd.runtime_id)
        .await
        .ok_or_else(|| {
            RuntimeError::Provider(format!("runtime '{}' not connected", cmd.runtime_id))
        })?;
    let call_id = cmd.call.call_id.clone();
    let call = cmd.call.call.clone();
    let runtime_id = cmd.runtime_id.clone();
    let sink = sink.clone();
    tokio::spawn(async move {
        let result = match transport.invoke(&call_id, call).await {
            Ok(r) => r,
            Err(e) => ToolResult::Err(ToolError {
                reason: e.to_string(),
            }),
        };
        let _ = send_outbound(
            &sink,
            ExecutorOutboundMessage {
                request_id: call_id.clone(),
                event: ExecutorEvent::ToolResult(ToolResultEvent {
                    runtime_id,
                    call_id,
                    result,
                }),
            },
        )
        .await;
    });
    Ok(())
}

async fn do_cancel_tool_call(
    cmd: &CancelToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
) -> Result<(), RuntimeError> {
    if let Some(reg) = connected_registry
        && let Some(transport) = reg.runtime_transport(&cmd.runtime_id).await
    {
        let _ = transport.cancel(&cmd.call_id).await;
    }
    Ok(())
}

async fn do_create(
    cmd: &CreateRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Creating).await;
    match create_core(registry, provider, &cmd.runtime_id, cmd.config.clone()).await {
        Ok(()) => {
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Running).await;
            Ok(())
        }
        Err(e) => {
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Failed).await;
            Err(e)
        }
    }
}

async fn do_destroy(
    cmd: &DestroyRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    let handle = registry.begin_stop(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Stopping).await;
    if let Some(h) = handle {
        let _ = h.stop().await;
    }
    registry.complete_stop(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Stopped).await;
    Ok(())
}

async fn do_restart(
    cmd: &RestartRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    let config = registry
        .get_config(&cmd.runtime_id)
        .await
        .ok_or_else(|| RuntimeError::NotFound(cmd.runtime_id.clone()))?;
    let old_handle = registry.begin_restart(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Creating).await;
    if let Some(h) = old_handle {
        let _ = h.stop().await;
    }
    match provider.create(&cmd.runtime_id, &config).await {
        Ok(handle) => {
            registry.complete_create(&cmd.runtime_id, handle).await?;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Running).await;
            Ok(())
        }
        Err(e) => {
            let _ = registry.mark_failed(&cmd.runtime_id).await;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Failed).await;
            Err(e)
        }
    }
}

async fn run_health_check(
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    max_restarts: u32,
) {
    let handles = registry.running_handles().await;
    for (id, handle) in handles {
        let healthy = matches!(handle.health_check().await, Ok(HealthStatus::Healthy));
        if healthy {
            continue;
        }
        let _ = registry.mark_failed(&id).await;
        let unsolicited = Uuid::new_v4().to_string();
        emit_state(sink, &unsolicited, &id, RuntimeState::Failed).await;

        let count = registry.get_restart_count(&id).await.unwrap_or(u32::MAX);
        if count >= max_restarts {
            continue;
        }
        if let Some(config) = registry.get_config(&id).await
            && let Ok(old) = registry.begin_restart(&id).await
        {
            emit_state(sink, &unsolicited, &id, RuntimeState::Creating).await;
            if let Some(h) = old {
                let _ = h.stop().await;
            }
            match provider.create(&id, &config).await {
                Ok(new_handle) => {
                    let _ = registry.complete_create(&id, new_handle).await;
                    emit_state(sink, &unsolicited, &id, RuntimeState::Running).await;
                }
                Err(_) => {
                    let _ = registry.mark_failed(&id).await;
                    emit_state(sink, &unsolicited, &id, RuntimeState::Failed).await;
                }
            }
        }
    }
}
