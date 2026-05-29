use crate::{
    connected_registry::{ConnectedRuntimeRegistry, RuntimeSink},
    error::{ExecutorError, RuntimeError},
    provider::{HealthStatus, RuntimeProvider},
    registry::RuntimeRegistry,
    runtime_listener::RuntimeListenerServer,
};
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, CommandFailedEvent, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand,
    ExecutorEvent, ExecutorInboundMessage, ExecutorOutboundMessage, RegisteredEvent,
    RestartRuntimeCmd, RuntimeState, RuntimeStateChangedEvent, RuntimesListedEvent, ToolCallCmd,
    ToolResultEvent,
};
use models::runtime::{
    CancelCallRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ToolCallRequest,
};
use std::sync::Arc;
use std::time::Duration;
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

        // Start runtime listener if configured
        if let (Some(listener), Some(conn_reg)) =
            (self.runtime_listener, connected_registry.clone())
        {
            let listener_sink = sink.clone();
            let listener_cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = listener_cancel.cancelled() => break,
                        result = listener.accept() => {
                            match result {
                                Ok(ws) => {
                                    let reg = conn_reg.clone();
                                    let srv_sink = listener_sink.clone();
                                    tokio::spawn(handle_runtime_connection(ws, reg, srv_sink));
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
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
        ExecutorCommand::ToolCall(cmd) => do_tool_call(cmd, connected_registry, sink, req).await,
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

async fn do_tool_call(
    cmd: &ToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
    _sink: &WsSink,
    _req: &str,
) -> Result<(), RuntimeError> {
    let reg = connected_registry
        .ok_or_else(|| RuntimeError::Provider("no runtime listener configured".to_string()))?;
    let msg = RuntimeInboundMessage::ToolCall(ToolCallRequest {
        call_id: cmd.call.call_id.clone(),
        call: cmd.call.call.clone(),
    });
    let json = serde_json::to_string(&msg).map_err(|e| RuntimeError::Provider(e.to_string()))?;
    reg.send_to(&cmd.runtime_id, json)
        .await
        .map_err(|e| RuntimeError::Provider(e.to_string()))
}

async fn do_cancel_tool_call(
    cmd: &CancelToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
) -> Result<(), RuntimeError> {
    let reg = connected_registry
        .ok_or_else(|| RuntimeError::Provider("no runtime listener configured".to_string()))?;
    let msg = RuntimeInboundMessage::CancelCall(CancelCallRequest {
        call_id: cmd.call_id.clone(),
    });
    let json = serde_json::to_string(&msg).map_err(|e| RuntimeError::Provider(e.to_string()))?;
    // Best-effort
    let _ = reg.send_to(&cmd.runtime_id, json).await;
    Ok(())
}

async fn handle_runtime_connection(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    registry: Arc<ConnectedRuntimeRegistry>,
    server_sink: WsSink,
) {
    let (ws_sink, mut ws_stream) = ws.split();
    let ws_sink: RuntimeSink = Arc::new(Mutex::new(ws_sink));

    // First message must be RuntimeReady
    let runtime_id = loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(RuntimeOutboundMessage::Ready(ev)) =
                    serde_json::from_str::<RuntimeOutboundMessage>(&text)
                {
                    break ev.runtime_id;
                }
            }
            _ => return,
        }
    };

    registry.register(runtime_id.clone(), ws_sink).await;

    // Process ToolCallResponse messages
    while let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(text)) = msg {
            if let Ok(RuntimeOutboundMessage::ToolCallResponse(resp)) =
                serde_json::from_str::<RuntimeOutboundMessage>(&text)
            {
                let event = ExecutorOutboundMessage {
                    request_id: resp.call_id.clone(),
                    event: ExecutorEvent::ToolResult(ToolResultEvent {
                        runtime_id: runtime_id.clone(),
                        call_id: resp.call_id,
                        result: resp.result,
                    }),
                };
                let _ = send_outbound(&server_sink, event).await;
            }
        } else {
            break;
        }
    }

    registry.remove(&runtime_id).await;
}

async fn do_create(
    cmd: &CreateRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    registry
        .begin_create(&cmd.runtime_id, cmd.config.clone())
        .await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Creating).await;
    match provider.create(&cmd.runtime_id, &cmd.config).await {
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
