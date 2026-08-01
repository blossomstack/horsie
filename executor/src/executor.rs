use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::{ExecutorError, RuntimeError},
    provider::RuntimeProvider,
    registry::RuntimeRegistry,
    runtime_listener::{AcceptedConn, RuntimeListenerServer},
    socket_transport::SocketRuntimeTransport,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{
    ExecutorEvent, ExecutorOutboundMessage, RuntimeConfig, RuntimeState, RuntimeStateChangedEvent,
};
use horsie_models::runtime::RuntimeOutboundMessage;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;

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

/// Fires with `runtime_id` after a runtime successfully registers (not on a
/// rejected collision). Lets a vendor that registers runtimes outside any
/// `create`/`attach` call (e.g. a user-launched daemon dialing in on its own)
/// learn about a newly (re)connected id without polling.
pub type ConnectHook = Arc<dyn Fn(String) + Send + Sync>;

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
/// after each successful registration with the `runtime_id`.
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

/// Handshake on an accepted runtime link, then register it as a direct transport.
/// Generic over the socket type so TCP and unix share one accept/handshake/frame path.
///
/// Public so a host that owns its own listener (e.g. the session server serving
/// runtime dial-backs as a WebSocket-upgrade route over its HTTP port) can drive
/// the same handshake/registration logic without going through
/// [`RuntimeListenerServer`]. `ws` is any already-upgraded WebSocket stream.
pub async fn handle_runtime_connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    registry: Arc<ConnectedRuntimeRegistry>,
    on_registered: Option<ConnectHook>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut stream) = ws.split();

    enum Handshake {
        Ready(String),
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
                            return Some(Handshake::Ready(ev.runtime_id));
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

    let runtime_id = match first {
        Ok(Some(Handshake::Ready(id))) => id,
        Ok(Some(Handshake::Provisioning(id))) => {
            // Provision phase: wait (much longer) for Ready or ProvisionFailed.
            let outcome = tokio::time::timeout(PROVISION_WINDOW, async {
                loop {
                    match stream.next().await {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                                Ok(RuntimeOutboundMessage::Ready(ev)) => {
                                    return Ok(ev.runtime_id);
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
        hook(runtime_id.clone());
    }
    // Deregister when the link drops so health checks observe the loss and a stale
    // transport never lingers (explicit destroy also removes it; double-remove is safe).
    let _ = closed.await;
    registry.remove(&runtime_id).await;
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

    async fn announce(addr: std::net::SocketAddr, runtime_id: &str) -> WsSinkPair {
        let (ws, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");
        let (mut sink, stream) = ws.split();
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id: runtime_id.to_string(),
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
        let listener =
            RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
                .await
                .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, registry.clone(), cancel.clone());

        let (_sink1, _stream1) = announce(addr, "dup-id").await;
        wait_registered(&registry, "dup-id").await;

        // A second connection announcing the SAME id must be rejected: its
        // socket closes, and the first transport stays registered.
        let (mut sink2, mut stream2) = announce(addr, "dup-id").await;
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
    async fn on_registered_hook_fires_with_id_once_per_registration() {
        let listener =
            RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
                .await
                .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook: ConnectHook = Arc::new(move |id: String| {
            hook_seen.lock().unwrap().push(id);
        });
        serve_runtime_connections_with_hook(listener, registry.clone(), cancel.clone(), Some(hook));

        let (_sink, _stream) = announce(addr, "rt-1").await;
        wait_registered(&registry, "rt-1").await;

        assert_eq!(seen.lock().unwrap().as_slice(), &["rt-1".to_string()]);
        cancel.cancel();
    }
}
