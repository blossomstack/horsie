//! `RuntimeTransport` implemented over a [`RuntimeVendorLink`].
//!
//! This is the seam that leaves the session layer untouched: `RuntimeClient`
//! wraps any `RuntimeTransport`, so routing a session's tool calls through a
//! vendor is a matter of stamping a `runtime_id` onto each relayed message.
//!
//! Nothing here understands what it is forwarding. The runtime protocol crosses
//! the vendor link whole, so a new runtime message needs no change in this file,
//! in the link, or in the vendor agent on the other end.

use crate::runtime_vendor::RuntimeVendorLink;
use async_trait::async_trait;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_models::runtime_vendor::{
    RuntimeRelayRequest, RuntimeVendorCommand, RuntimeVendorEvent,
};
use horsie_runtime_client::{RuntimeTransport, TransportError};
use std::sync::Arc;

pub struct RuntimeVendorTransport {
    link: Arc<RuntimeVendorLink>,
    runtime_id: String,
}

impl RuntimeVendorTransport {
    #[must_use]
    pub fn new(link: Arc<RuntimeVendorLink>, runtime_id: String) -> Self {
        Self { link, runtime_id }
    }

    /// Map a link error onto the transport's vocabulary. `Disconnected` is
    /// load-bearing: `RuntimeClient` latches on exactly that variant, and a
    /// mislabelled error would leave a session reusing a dead client for every
    /// later turn.
    fn transport_error(&self, message: String) -> TransportError {
        if !self.link.is_connected() || message.contains("disconnect") {
            TransportError::Disconnected
        } else {
            TransportError::SendFailed(message)
        }
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
        let event = self
            .link
            .request(self.addressed(message))
            .await
            .map_err(|e| self.transport_error(e))?;
        match event {
            RuntimeVendorEvent::Runtime(ev) => Ok(ev.message),
            RuntimeVendorEvent::Ready(_)
            | RuntimeVendorEvent::CreateRuntime(_)
            | RuntimeVendorEvent::AttachRuntime(_)
            | RuntimeVendorEvent::StopRuntime(_)
            | RuntimeVendorEvent::DeleteRuntime(_)
            | RuntimeVendorEvent::QueryRuntimes(_)
            | RuntimeVendorEvent::RequestFailed(_)
            | RuntimeVendorEvent::RuntimeStateChanged(_) => Err(TransportError::SendFailed(
                "the vendor answered a relayed runtime request with a lifecycle event".to_string(),
            )),
        }
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.link
            .send_oneway(self.addressed(message))
            .await
            .map_err(|e| self.transport_error(e))
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
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::runtime::{
        BashInput, ToolCall, ToolCallResponse, ToolError, ToolOutput, ToolResult,
    };
    use horsie_models::runtime_vendor::{
        RuntimeRelayResponse, RuntimeVendorCapabilities, RuntimeVendorInboundMessage,
        RuntimeVendorOutboundMessage, RuntimeVendorReady,
    };
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn boot_agent(answer_calls: bool, stdout: &'static str) -> Arc<RuntimeVendorLink> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        tokio::spawn(async move {
            let boot = RuntimeVendorOutboundMessage {
                request_id: "boot".to_string(),
                event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
                    vendor_name: "v".to_string(),
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
                            }),
                        }),
                    }),
                };
                agent
                    .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .unwrap();
            }
        });
        RuntimeVendorLink::start(server).await.unwrap()
    }

    fn bash() -> ToolCall {
        ToolCall::Bash(BashInput {
            command: "echo hi".to_string(),
            timeout_secs: None,
            workspace: None,
        })
    }

    #[tokio::test]
    async fn invoke_round_trips_a_tool_call_through_the_link() {
        let link = boot_agent(true, "hello-from-agent").await;
        let transport = RuntimeVendorTransport::new(link, "rt-1".to_string());
        let result = transport
            .invoke("call-1", "agent-1", bash())
            .await
            .expect("tool call");
        match result {
            ToolResult::Ok(out) => assert_eq!(out.stdout, "hello-from-agent"),
            ToolResult::Err(ToolError { reason }) => panic!("expected success, got {reason}"),
        }
    }

    #[tokio::test]
    async fn invoke_reports_disconnected_so_runtime_client_latches() {
        let link = boot_agent(false, "").await;
        let transport = RuntimeVendorTransport::new(link, "rt-1".to_string());
        let err = transport
            .invoke("call-1", "agent-1", bash())
            .await
            .expect_err("a dead link must fail the call");
        assert!(
            matches!(err, TransportError::Disconnected),
            "RuntimeClient only latches on Disconnected, got {err:?}"
        );
    }
}
