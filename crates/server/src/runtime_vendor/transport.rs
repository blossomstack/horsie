//! `RuntimeTransport` implemented over whichever link a vendor holds *now*.
//!
//! This is the seam that leaves the session layer untouched: `RuntimeClient`
//! wraps any `RuntimeTransport`, so routing a session's tool calls through a
//! vendor is a matter of stamping a `runtime_id` onto each relayed message.
//!
//! Nothing here understands what it is forwarding. The runtime protocol crosses
//! the vendor link whole, so a new runtime message needs no change in this file,
//! in the link, or in the vendor process on the other end.
//!
//! **The link is resolved per call, never held.** A vendor process that reconnects
//! comes back on a brand-new [`WebsocketRuntimeVendor`]; the one it was reached on
//! before is a corpse forever. A transport that captured that `Arc` would fail
//! every later call, and since a client is acquired once per *run* and baked
//! into the toolbox, a reconnect mid-turn left the agent loop retrying against a
//! dead socket until it burned through `max_iterations`. Resolving through the
//! registry on each call costs one `RwLock` read against a round trip to a
//! sandbox, and makes a reconnect invisible to a turn already in flight.

use crate::runtime_vendor::WebsocketRuntimeVendor;
use async_trait::async_trait;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_models::runtime_vendor::{
    RuntimeRelayRequest, RuntimeVendorCommand, RuntimeVendorEvent,
};
use horsie_runtime_host::{RuntimeTransport, TransportError};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// How long a call waits for an absent vendor to come back before failing.
///
/// Short on purpose. It only has to cover the re-dial after a blip (the agent's
/// first retry is well under a second away), and a call that gives up is not the
/// end of the road: the next one resolves again, so the loop heals either way.
const RELINK_WAIT: Duration = Duration::from_secs(5);

/// How often the wait re-checks the registry. Cheap enough to poll: a read lock
/// on a map that is only written when an agent connects or disconnects.
const RELINK_POLL: Duration = Duration::from_millis(100);

pub struct RuntimeVendorTransport {
    vendors: crate::runtime_vendor::WebsocketVendorTable,
    vendor_name: String,
    runtime_id: String,
    /// Flips when the vendor process this runtime lives behind goes away.
    ///
    /// `None` for a vendor with nothing watching, which is not the same as a
    /// vendor that is fine — see [`RuntimeTransport::closed`] for why the two
    /// must not be conflated.
    closed: Option<tokio::sync::watch::Receiver<bool>>,
}

impl RuntimeVendorTransport {
    #[must_use]
    pub fn new(
        vendors: crate::runtime_vendor::WebsocketVendorTable,
        vendor_name: String,
        runtime_id: String,
    ) -> Self {
        Self {
            vendors,
            vendor_name,
            runtime_id,
            closed: None,
        }
    }

    /// The same transport, reporting a closure when `closed` flips.
    #[must_use]
    pub fn watching(mut self, closed: tokio::sync::watch::Receiver<bool>) -> Self {
        self.closed = Some(closed);
        self
    }

    /// The vendor's live link, or `None` while it is away.
    fn current_link(&self) -> Option<Arc<WebsocketRuntimeVendor>> {
        let vendors = self.vendors.lock().unwrap_or_else(PoisonError::into_inner);
        vendors
            .get(&self.vendor_name)
            .filter(|link| link.is_reachable())
            .cloned()
    }

    /// The link to send this call over, waiting out a brief absence.
    async fn link(&self) -> Result<Arc<WebsocketRuntimeVendor>, TransportError> {
        if let Some(link) = self.current_link() {
            return Ok(link);
        }
        let deadline = Instant::now() + RELINK_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(RELINK_POLL).await;
            if let Some(link) = self.current_link() {
                return Ok(link);
            }
        }
        // Never `Disconnected`: that variant means "this transport can never
        // work again", and this one has no such state — the vendor may well be
        // back before the model asks for its next tool.
        Err(TransportError::SendFailed(format!(
            "runtime vendor '{}' is not connected",
            self.vendor_name
        )))
    }

    fn addressed(&self, message: RuntimeInboundMessage) -> RuntimeVendorCommand {
        RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: self.runtime_id.clone(),
            message,
        })
    }
}

#[async_trait]
impl RuntimeTransport for RuntimeVendorTransport {
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        // Resolved once, before sending. A call that fails *after* it was
        // written is never re-sent on a fresh link: the message may already have
        // reached the runtime and run, and `bash` is not idempotent. Re-linking
        // is for the next call, not for retrying this one.
        let link = self.link().await?;
        let event = link
            .request(self.addressed(message))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))?;
        match event {
            RuntimeVendorEvent::Runtime(ev) => Ok(ev.message),
            RuntimeVendorEvent::Ready(_)
            | RuntimeVendorEvent::CreateRuntime(_)
            | RuntimeVendorEvent::GetRuntime(_)
            | RuntimeVendorEvent::HibernateRuntime(_)
            | RuntimeVendorEvent::DeleteRuntime(_)
            | RuntimeVendorEvent::QueryRuntimes(_)
            | RuntimeVendorEvent::RequestFailed(_)
            | RuntimeVendorEvent::RuntimeStateChanged(_) => Err(TransportError::SendFailed(
                "the vendor answered a relayed runtime request with a lifecycle event".to_string(),
            )),
        }
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.link()
            .await?
            .send_oneway(self.addressed(message))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }

    async fn closed(&self) {
        horsie_runtime_host::closed_when(self.closed.clone()).await;
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

    /// A vendor table of its own. Handles minted against it resolve back to
    /// whatever the test publishes.
    fn test_links() -> crate::runtime_vendor::WebsocketVendorTable {
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    use crate::auth::Principal;
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::runtime::{
        BashInput, ToolCall, ToolCallResponse, ToolError, ToolOutput, ToolResult,
    };
    use horsie_models::runtime_vendor::{
        RuntimeRelayResponse, RuntimeVendorCapabilities, RuntimeVendorInboundMessage,
        RuntimeVendorOutboundMessage, RuntimeVendorReady,
    };
    use std::collections::HashMap;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn boot_agent(answer_calls: bool, stdout: &'static str) -> Arc<WebsocketRuntimeVendor> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        tokio::spawn(async move {
            let boot = RuntimeVendorOutboundMessage {
                request_id: "boot".to_string(),
                event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
                    vendor_name: "v".to_string(),
                    instance_id: "v-instance".to_string(),
                    capabilities: RuntimeVendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            };
            agent
                .send(Message::Text(serde_json::to_string(&boot).unwrap().into()))
                .await
                .unwrap();
            while let Some(Ok(Message::Text(text))) = agent.next().await {
                if !answer_calls {
                    drop(agent);
                    return;
                }
                let inbound: RuntimeVendorInboundMessage = serde_json::from_str(&text).unwrap();
                let call_id = match &inbound.command {
                    RuntimeVendorCommand::Runtime(relay) => match &relay.message {
                        RuntimeInboundMessage::ToolCall(c) => c.call_id.clone(),
                        other => panic!("unexpected relayed message {other:?}"),
                    },
                    other => panic!("unexpected command {other:?}"),
                };
                let reply = RuntimeVendorOutboundMessage {
                    request_id: inbound.request_id,
                    event: RuntimeVendorEvent::Runtime(RuntimeRelayResponse {
                        runtime_id: "rt-1".to_string(),
                        message: RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                            call_id,
                            result: ToolResult::Ok(ToolOutput {
                                stdout: stdout.to_string(),
                                stderr: String::new(),
                                exit_code: 0,
                                artifacts: Vec::new(),
                            }),
                            hooks: Vec::new(),
                        }),
                    }),
                };
                agent
                    .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .unwrap();
            }
        });
        WebsocketRuntimeVendor::start(server, Principal::Anonymous, test_links())
            .await
            .unwrap()
    }

    /// The table the transport reads, holding one vendor.
    fn published(
        name: &str,
        link: Arc<WebsocketRuntimeVendor>,
    ) -> crate::runtime_vendor::WebsocketVendorTable {
        let mut map = HashMap::new();
        map.insert(name.to_string(), link);
        Arc::new(std::sync::Mutex::new(map))
    }

    fn transport(vendors: &crate::runtime_vendor::WebsocketVendorTable) -> RuntimeVendorTransport {
        RuntimeVendorTransport::new(vendors.clone(), "v".to_string(), "rt-1".to_string())
    }

    fn bash() -> ToolCall {
        ToolCall::Bash(BashInput {
            command: "echo hi".to_string(),
            timeout_secs: None,
        })
    }

    #[tokio::test]
    async fn invoke_round_trips_a_tool_call_through_the_link() {
        let vendors = published("v", boot_agent(true, "hello-from-agent").await);
        let result = transport(&vendors)
            .invoke("call-1", "agent-1", bash())
            .await
            .expect("tool call");
        match result.0 {
            ToolResult::Ok(out) => assert_eq!(out.stdout, "hello-from-agent"),
            ToolResult::Err(ToolError { reason }) => panic!("expected success, got {reason}"),
        }
    }

    /// The bug this transport exists to prevent: an agent loop acquires its
    /// client once per run, so a vendor that reconnects mid-turn has to be
    /// picked up by the *same* transport. Holding the link made every later tool
    /// call in that turn fail on a socket nobody could revive.
    #[tokio::test]
    async fn a_call_after_a_reconnect_goes_over_the_new_link() {
        let dead = boot_agent(false, "").await;
        let vendors = published("v", dead.clone());
        let transport = transport(&vendors);

        // The link the transport would have captured is gone.
        assert!(transport.invoke("call-1", "agent-1", bash()).await.is_err());

        // The same agent comes back; the registry swaps in its new link exactly
        // as `RuntimeVendorRegistry::register` does.
        let live = boot_agent(true, "back-again").await;
        assert!(!Arc::ptr_eq(&dead, &live));
        vendors
            .lock()
            .unwrap()
            .insert("v".to_string(), live.clone());

        let result = transport
            .invoke("call-2", "agent-1", bash())
            .await
            .expect("a reconnected vendor must serve the transport that outlived its link");
        match result.0 {
            ToolResult::Ok(out) => assert_eq!(out.stdout, "back-again"),
            ToolResult::Err(ToolError { reason }) => panic!("expected success, got {reason}"),
        }
    }

    /// A vendor that is away is a *transient* failure. Reporting `Disconnected`
    /// would be a lie now: this transport has no state a reconnect cannot fix,
    /// and that variant is what latches a `RuntimeClient` off for good.
    #[tokio::test]
    async fn an_absent_vendor_is_never_reported_as_disconnected() {
        let vendors: crate::runtime_vendor::WebsocketVendorTable =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let err = transport(&vendors)
            .invoke("call-1", "agent-1", bash())
            .await
            .expect_err("a vendor that is not there cannot serve a call");
        assert!(
            matches!(err, TransportError::SendFailed(_)),
            "an absent vendor is retryable, not terminal: {err:?}"
        );
    }

    /// The wait exists so an agent that re-dials during a tool call stays
    /// invisible to the model — the call takes a moment longer and succeeds.
    #[tokio::test]
    async fn a_call_waits_for_a_vendor_that_is_still_re_dialling() {
        let vendors: crate::runtime_vendor::WebsocketVendorTable =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let arriving = vendors.clone();
        tokio::spawn(async move {
            let link = boot_agent(true, "late-arrival").await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            arriving.lock().unwrap().insert("v".to_string(), link);
        });

        let result = transport(&vendors)
            .invoke("call-1", "agent-1", bash())
            .await
            .expect("the call must wait out a short absence rather than fail immediately");
        match result.0 {
            ToolResult::Ok(out) => assert_eq!(out.stdout, "late-arrival"),
            ToolResult::Err(ToolError { reason }) => panic!("expected success, got {reason}"),
        }
    }
}
