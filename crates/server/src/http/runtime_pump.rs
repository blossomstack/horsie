//! The accepted dial-back, as a pump between one socket and two topics.
//!
//! A runtime's connection lands wherever DNS put it, which is not necessarily
//! the node its session is running on. Rather than move the connection — a live
//! socket cannot be serialized, and every scheme for reaching across to one
//! fails when the far node dies — the connection terminates here and the session
//! reaches it by name.
//!
//! So this node owns exactly two loops: whatever arrives on `rt:<s>:<i>:in` is
//! written to the socket, and whatever the socket says is published to
//! `rt:<s>:<i>:out`. Nothing is registered globally, and no other node learns
//! this one's identity.
//!
//! **The incarnation comes from the token, never from the connection.** A
//! sandbox announces its own runtime id in its handshake, and a sandbox left
//! over from an earlier provision announces the same one — they are the same
//! session. What separates them is the incarnation in the signed token, so
//! reading it from anywhere else would put both sandboxes on one topic and have
//! both run every tool call.

use crate::bus::{Bus, topics};
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::RuntimeOutboundMessage;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

/// Run the two loops until either end goes away.
///
/// Returns when the socket closes or the in-topic ends, so the caller's spawned
/// task finishes with the connection rather than outliving it.
pub async fn pump<S>(ws: WebSocketStream<S>, bus: Arc<dyn Bus>, session: Uuid, incarnation: &str)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Subscribed before a single frame is written anywhere. A session that
    // publishes a tool call the instant it sees `Ready` would otherwise publish
    // into a topic this node is not yet reading, and the bus keeps nothing for a
    // subscriber that has not arrived — the call would simply not exist.
    let inbound = match topics::runtime_in(bus.clone(), session, incarnation)
        .subscribe()
        .await
    {
        Ok(reader) => reader,
        Err(e) => {
            tracing::error!(error = %e, %session, "a runtime dialled in but its topic could not be read");
            return;
        }
    };
    let outbound = topics::runtime_out(bus, session, incarnation);

    let (mut sink, mut stream) = ws.split();

    let to_socket = tokio::spawn(async move {
        let mut inbound = inbound;
        while let Some(message) = inbound.recv().await {
            let json = match serde_json::to_string(&message) {
                Ok(json) => json,
                // One unserialisable message must not take the connection with
                // it: the runtime is fine, and the next call will be too.
                Err(e) => {
                    tracing::warn!(error = %e, "a runtime message could not be encoded; skipping it");
                    continue;
                }
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Everything the socket says goes onto the out topic, handshake included:
    // `Ready`, `Provisioning` and `ProvisionFailed` are already
    // `RuntimeOutboundMessage` variants, so readiness needs no second mechanism
    // — it is the first frame a session's subscription sees.
    while let Some(Ok(Message::Text(text))) = stream.next().await {
        let Ok(message) = serde_json::from_str::<RuntimeOutboundMessage>(&text) else {
            tracing::warn!("a runtime sent a frame that did not decode; skipping it");
            continue;
        };
        if let Err(e) = outbound.publish(&message).await {
            tracing::warn!(error = %e, "a runtime reply could not be published");
        }
    }

    to_socket.abort();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;
    use horsie_models::runtime::{CancelCallRequest, RuntimeInboundMessage};
    use tokio_tungstenite::tungstenite::protocol::Role;

    fn cancel(call_id: &str) -> RuntimeInboundMessage {
        RuntimeInboundMessage::CancelCall(CancelCallRequest {
            call_id: call_id.to_string(),
        })
    }

    /// The fence. A sandbox from an earlier provision announces the same runtime
    /// id as the live one — they are the same session — so the only thing that
    /// separates them is the incarnation in the signed token. Reading it from
    /// the connection instead would put both on one topic, and both would run
    /// every tool call.
    #[tokio::test]
    async fn a_pump_only_carries_the_incarnation_its_token_named() {
        let bus: Arc<dyn Bus> = Arc::new(MemoryBus::new());
        let session = Uuid::new_v4();

        let (server, client) = tokio::io::duplex(64 * 1024);
        let server_ws = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut client_ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        let pumped_bus = bus.clone();
        tokio::spawn(async move { pump(server_ws, pumped_bus, session, "1").await });
        // Give the pump its subscription before anything is published: the bus
        // keeps nothing for a subscriber that has not arrived yet.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        topics::runtime_in(bus.clone(), session, "2")
            .publish(&cancel("from-another-provision"))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), client_ws.next())
                .await
                .is_err(),
            "a pump fenced to incarnation 1 must not carry incarnation 2's traffic"
        );

        // And it does carry its own, so the test above is not passing because
        // the pump is simply broken.
        topics::runtime_in(bus, session, "1")
            .publish(&cancel("for-this-provision"))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), client_ws.next())
            .await
            .expect("its own incarnation's traffic must arrive")
            .expect("a frame")
            .unwrap();
        assert!(frame.to_text().unwrap().contains("for-this-provision"));
    }
}
