//! Accepting runtime dial-backs.
//!
//! A vendor agent binds a listener its runtimes dial; this drives the
//! handshake and registers each connection's transport. The server no longer
//! accepts runtime connections at all — runtimes talk only to their agent.

use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    runtime_listener::{AcceptedStream, RuntimeListenerServer},
    socket_transport::SocketRuntimeTransport,
    vendor::note,
};
use futures_util::StreamExt;
use horsie_models::runtime::RuntimeOutboundMessage;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// How long a runtime may spend in provision steps (e.g. cloning) between its
/// Provisioning announce and Ready before the link is dropped.
const PROVISION_WINDOW: Duration = Duration::from_secs(900);

/// How long a peer may take to complete the WebSocket handshake. Bounded so a
/// connection that never speaks costs one task, not a listener.
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(10);

/// Consecutive `accept()` failures that mean the listener itself is gone rather
/// than one peer misbehaving. Below this, accepting simply carries on.
const FATAL_ACCEPT_FAILURES: usize = 10;

/// How long to wait after a failed accept, so a permanently broken listener
/// cannot spin this loop hot on its way to [`FATAL_ACCEPT_FAILURES`].
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Accept runtime connections on `listener` and register each as a direct transport,
/// until `cancel` fires. Used by CLI mode (which drives lifecycle via
/// [`InMemExecutorTransport`](crate::InMemExecutorTransport)) to run the listener loop.
///
/// The loop outlives bad peers by construction. It ends only on `cancel` or on a
/// listener that has failed [`FATAL_ACCEPT_FAILURES`] times in a row — and that
/// case cancels the token rather than returning quietly, because an agent that
/// keeps serving with no listener spawns runtimes that can never dial in.
pub fn serve_runtime_connections(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut consecutive_failures = 0usize;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(AcceptedStream::Tcp(stream)) => {
                        consecutive_failures = 0;
                        tokio::spawn(upgrade_and_serve(stream, registry.clone()));
                    }
                    Ok(AcceptedStream::Unix(stream)) => {
                        consecutive_failures = 0;
                        tokio::spawn(upgrade_and_serve(stream, registry.clone()));
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        note(&format!(
                            "vendor agent: accepting a runtime connection failed \
                             ({consecutive_failures}/{FATAL_ACCEPT_FAILURES}): {e}"
                        ));
                        if consecutive_failures >= FATAL_ACCEPT_FAILURES {
                            note(
                                "vendor agent: the runtime listener has stopped accepting; \
                                 shutting down so this agent can be restarted rather than \
                                 serving sessions no runtime can reach",
                            );
                            cancel.cancel();
                            break;
                        }
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                }
            }
        }
        // Dropping `listener` here unlinks the unix socket (its Drop impl).
    });
}

/// Complete the WebSocket handshake for one accepted socket, then serve it.
///
/// Off the accept path on purpose: a runtime child that is killed between
/// `connect()` and its first byte fails here, where it costs one task, instead
/// of taking the listener down with it.
async fn upgrade_and_serve<S>(stream: S, registry: Arc<ConnectedRuntimeRegistry>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match tokio::time::timeout(HANDSHAKE_WINDOW, accept_async(stream)).await {
        Ok(Ok(ws)) => handle_runtime_connection(ws, registry).await,
        Ok(Err(e)) => note(&format!(
            "vendor agent: a runtime connection failed its handshake: {e}"
        )),
        Err(_) => note("vendor agent: a runtime connection never completed its handshake"),
    }
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

    /// A runtime child that dies between `connect()` and the WebSocket
    /// handshake must cost only its own connection: the agent stays able to
    /// accept the runtimes it spawns next, and the socket file it told them to
    /// dial must still be there.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_peer_that_hangs_up_before_the_handshake_leaves_the_listener_alive() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vendor-runtimes.sock");
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
            .await
            .unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, registry.clone(), cancel.clone());

        drop(tokio::net::UnixStream::connect(&sock).await.unwrap());
        tokio::time::sleep(StdDuration::from_millis(300)).await;

        assert!(
            sock.exists(),
            "the socket file was unlinked, so every runtime spawned from here on \
             dials a path that no longer exists"
        );
        assert!(
            tokio::net::UnixStream::connect(&sock).await.is_ok(),
            "the next runtime must still be able to dial in"
        );
        cancel.cancel();
    }

    /// The same, one layer up: after a peer hangs up mid-handshake, a real
    /// runtime still gets registered. Bad peers must not cost registrations.
    #[tokio::test]
    async fn a_bad_peer_does_not_stop_the_next_runtime_from_registering() {
        let listener =
            RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
                .await
                .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, registry.clone(), cancel.clone());

        // Connects, speaks no WebSocket, hangs up.
        drop(tokio::net::TcpStream::connect(addr).await.unwrap());
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        let (_sink, _stream) = announce(addr, "after-bad-peer").await;
        wait_registered(&registry, "after-bad-peer").await;
        cancel.cancel();
    }

    /// Shutdown still takes the socket file with it, so the next agent does not
    /// inherit a path nothing is listening on.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_unlinks_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vendor-runtimes-1.sock");
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        serve_runtime_connections(
            listener,
            Arc::new(ConnectedRuntimeRegistry::new()),
            cancel.clone(),
        );
        assert!(sock.exists());

        cancel.cancel();
        for _ in 0..50 {
            if !sock.exists() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        panic!("the socket file outlived the listener");
    }
}
