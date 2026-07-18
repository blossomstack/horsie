use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::{ExecutorError, RuntimeError},
    provider::{HealthStatus, RuntimeProvider},
    registry::RuntimeRegistry,
    runtime_listener::{AcceptedConn, RuntimeListenerServer},
    socket_transport::SocketRuntimeTransport,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{
    CancelToolCallCmd, CommandFailedEvent, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand,
    ExecutorEvent, ExecutorInboundMessage, ExecutorOutboundMessage, RegisteredEvent,
    RestartRuntimeCmd, RuntimeConfig, RuntimeState, RuntimeStateChangedEvent, RuntimesListedEvent,
    ToolCallCmd, ToolResultEvent,
};
use horsie_models::runtime::{RuntimeOutboundMessage, ToolError, ToolResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a runtime may spend in provision steps (e.g. cloning) between its
/// Provisioning announce and Ready before the executor drops the link.
const PROVISION_WINDOW: Duration = Duration::from_secs(900);

type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
            Message,
        >,
    >,
>;

/// Fires `(runtime_id, workdir)` after a runtime successfully registers (not
/// on a rejected collision). Lets a vendor that registers runtimes outside
/// any `create`/`attach` call (e.g. a user-launched daemon dialing in on its
/// own) learn about a newly (re)connected id without polling.
pub type ConnectHook = Arc<dyn Fn(String, String) + Send + Sync>;

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
    serve_runtime_connections_with_hook(listener, registry, cancel, None)
}

/// Like [`serve_runtime_connections`], but `on_registered` (if given) fires
/// after each successful registration with `(runtime_id, workdir)`.
pub fn serve_runtime_connections_with_hook(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
    on_registered: Option<ConnectHook>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(AcceptedConn::Tcp(ws)) => {
                        tokio::spawn(handle_runtime_connection(
                            ws,
                            registry.clone(),
                            on_registered.clone(),
                        ));
                    }
                    Ok(AcceptedConn::Unix(ws)) => {
                        tokio::spawn(handle_runtime_connection(
                            ws,
                            registry.clone(),
                            on_registered.clone(),
                        ));
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
    on_registered: Option<ConnectHook>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut stream) = ws.split();

    enum Handshake {
        Ready(String, String),
        Provisioning(String),
    }

    // First message must arrive within a bounded window so a peer that connects
    // but never announces itself can't leak this task forever.
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                        Ok(RuntimeOutboundMessage::Ready(ev)) => {
                            return Some(Handshake::Ready(ev.runtime_id, ev.workdir));
                        }
                        Ok(RuntimeOutboundMessage::Provisioning(ev)) => {
                            return Some(Handshake::Provisioning(ev.runtime_id));
                        }
                        _ => {}
                    }
                }
                _ => return None,
            }
        }
    })
    .await;

    let (runtime_id, workdir) = match first {
        Ok(Some(Handshake::Ready(id, workdir))) => (id, workdir),
        Ok(Some(Handshake::Provisioning(id))) => {
            // Provision phase: wait (much longer) for Ready or ProvisionFailed.
            let outcome = tokio::time::timeout(PROVISION_WINDOW, async {
                loop {
                    match stream.next().await {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                                Ok(RuntimeOutboundMessage::Ready(ev)) => {
                                    return Ok((ev.runtime_id, ev.workdir));
                                }
                                Ok(RuntimeOutboundMessage::ProvisionFailed(ev)) => {
                                    return Err(ev.message);
                                }
                                _ => {}
                            }
                        }
                        _ => return Err("runtime disconnected during provisioning".to_string()),
                    }
                }
            })
            .await;
            match outcome {
                Ok(Ok(ready)) => ready,
                Ok(Err(message)) => {
                    registry.fail_pending(&id, message).await;
                    return;
                }
                Err(_) => {
                    registry
                        .fail_pending(&id, "timed out during provisioning".to_string())
                        .await;
                    return;
                }
            }
        }
        // Timed out, stream closed, or garbage before an announce — drop the link.
        Ok(None) | Err(_) => return,
    };

    // Check BEFORE building the transport: `SocketRuntimeTransport::from_split`
    // unconditionally spawns a reader task that owns `stream` until the
    // socket itself closes, so rejecting *after* building it would leak that
    // task (dropping the transport handle alone doesn't stop it). A cheap
    // pre-check here means the common case (a duplicate label dialing in
    // well after the first is registered) drops `sink`/`stream` directly —
    // no task ever spawned, socket closes immediately.
    if registry.runtime_transport(&runtime_id).await.is_some() {
        return;
    }
    let (transport, closed) = SocketRuntimeTransport::from_split(sink, stream);
    if !registry
        .try_register_transport(runtime_id.clone(), Arc::new(transport))
        .await
    {
        // The narrow remaining race (two connections announcing the same id
        // within the same instant, both passing the check above before
        // either registers): `try_register_transport` is still the atomic
        // source of truth, so the loser is never reachable via
        // `runtime_transport()` — correctness holds. Its reader task isn't
        // proactively closed here, but it's inert (nothing will ever poll
        // it) and exits on its own once its peer disconnects.
        return;
    }
    if let Some(hook) = &on_registered {
        hook(runtime_id.clone(), workdir);
    }
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
        // Stop-preserve: the process side is identical to destroy (kill the child);
        // preservation is the caller's on-disk state, which the executor never owns.
        // Kept as a distinct wire signal so vendors with richer lifecycles (pause a
        // cloud sandbox, stop a container) can diverge without a protocol change.
        ExecutorCommand::StopRuntime(cmd) => {
            let destroy = DestroyRuntimeCmd {
                runtime_id: cmd.runtime_id.clone(),
            };
            do_destroy(&destroy, registry, sink, req).await
        }
        // Attach: a local process cannot resume in place, so revive by provisioning
        // a fresh child against the preserved config.
        ExecutorCommand::AttachRuntime(cmd) => {
            let create = CreateRuntimeCmd {
                runtime_id: cmd.runtime_id.clone(),
                config: cmd.config.clone(),
            };
            do_create(&create, registry, provider, sink, req).await
        }
        // Delete: the owning session is gone; this executor's choice is to tear the
        // process down (the user's workspace is never touched).
        ExecutorCommand::DeleteRuntime(cmd) => {
            let destroy = DestroyRuntimeCmd {
                runtime_id: cmd.runtime_id.clone(),
            };
            do_destroy(&destroy, registry, sink, req).await
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::runtime_listener::RuntimeEndpoint;
    use futures_util::SinkExt;
    use horsie_models::runtime::RuntimeReady;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration as StdDuration;
    use tokio_tungstenite::connect_async;

    async fn announce(addr: std::net::SocketAddr, runtime_id: &str, workdir: &str) -> WsSinkPair {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.expect("connect");
        let (mut sink, stream) = ws.split();
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id: runtime_id.to_string(),
            workdir: workdir.to_string(),
        }))
        .unwrap();
        sink.send(Message::Text(ready.into())).await.unwrap();
        (sink, stream)
    }

    type WsSinkPair = (
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    );

    async fn wait_registered(registry: &ConnectedRuntimeRegistry, id: &str) {
        for _ in 0..50 {
            if registry.runtime_transport(id).await.is_some() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        panic!("'{id}' never registered within 1s");
    }

    #[tokio::test]
    async fn duplicate_runtime_id_is_rejected_without_disturbing_the_live_one() {
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, registry.clone(), cancel.clone());

        let (_sink1, _stream1) = announce(addr, "dup-id", "/one").await;
        wait_registered(&registry, "dup-id").await;

        // A second connection announcing the SAME id must be rejected: its
        // socket closes, and the first transport stays registered.
        let (mut sink2, mut stream2) = announce(addr, "dup-id", "/two").await;
        let closed = tokio::time::timeout(StdDuration::from_secs(2), stream2.next()).await;
        assert!(
            matches!(closed, Ok(None) | Ok(Some(Err(_)))),
            "expected the duplicate connection to be closed, got {closed:?}"
        );
        let _ = sink2.close().await;
        assert!(
            registry.runtime_transport("dup-id").await.is_some(),
            "the original transport must still be registered"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn on_registered_hook_fires_with_id_and_workdir_once_per_registration() {
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        let seen: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook: ConnectHook = Arc::new(move |id: String, workdir: String| {
            hook_seen.lock().unwrap().push((id, workdir));
        });
        serve_runtime_connections_with_hook(listener, registry.clone(), cancel.clone(), Some(hook));

        let (_sink, _stream) = announce(addr, "rt-1", "/proj").await;
        wait_registered(&registry, "rt-1").await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[("rt-1".to_string(), "/proj".to_string())]
        );
        cancel.cancel();
    }
}
