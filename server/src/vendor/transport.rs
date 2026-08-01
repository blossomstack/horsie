//! `RuntimeTransport` implemented over a [`VendorLink`].
//!
//! This is the seam that leaves the session layer untouched: `RuntimeClient`
//! wraps any `RuntimeTransport`, so routing a session's tool calls through a
//! vendor agent is a matter of stamping a `runtime_id` onto each command.

use crate::vendor::VendorLink;
use async_trait::async_trait;
use horsie_models::runtime::{
    PluginSkill, ScanRequest, SessionStartRequest, ToolCall, ToolCallRequest, ToolResult,
    WorkspaceScan,
};
use horsie_models::vendor::{
    VendorCancelToolCall, VendorCommand, VendorEvent, VendorScanWorkspace, VendorSessionStart,
    VendorToolCall,
};
use horsie_runtime_client::{RuntimeTransport, TransportError};
use std::sync::Arc;

pub struct VendorRuntimeTransport {
    link: Arc<VendorLink>,
    runtime_id: String,
}

impl VendorRuntimeTransport {
    #[must_use]
    pub fn new(link: Arc<VendorLink>, runtime_id: String) -> Self {
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

    async fn request(&self, command: VendorCommand) -> Result<VendorEvent, TransportError> {
        self.link
            .request(command)
            .await
            .map_err(|e| self.transport_error(e))
    }
}

#[async_trait]
impl RuntimeTransport for VendorRuntimeTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let event = self
            .request(VendorCommand::ToolCall(VendorToolCall {
                runtime_id: self.runtime_id.clone(),
                call: ToolCallRequest {
                    call_id: call_id.to_string(),
                    call,
                },
            }))
            .await?;
        match event {
            VendorEvent::ToolResult(ev) => Ok(ev.result),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ScanResult(_)
            | VendorEvent::SessionStartResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a tool call with the wrong event".to_string(),
            )),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        self.link
            .send_oneway(VendorCommand::CancelToolCall(VendorCancelToolCall {
                runtime_id: self.runtime_id.clone(),
                call_id: call_id.to_string(),
            }))
            .await
            .map_err(|e| self.transport_error(e))
    }

    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError> {
        let event = self
            .request(VendorCommand::ScanWorkspace(VendorScanWorkspace {
                runtime_id: self.runtime_id.clone(),
                request: ScanRequest {
                    call_id: call_id.to_string(),
                    workspace,
                    instruction_candidates,
                    skills_glob,
                    include_shared,
                },
            }))
            .await?;
        match event {
            VendorEvent::ScanResult(ev) => Ok((ev.response.workspaces, ev.response.shared_skills)),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ToolResult(_)
            | VendorEvent::SessionStartResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a scan with the wrong event".to_string(),
            )),
        }
    }

    async fn run_session_start(&self, call_id: &str) -> Result<String, TransportError> {
        let event = self
            .request(VendorCommand::SessionStart(VendorSessionStart {
                runtime_id: self.runtime_id.clone(),
                request: SessionStartRequest {
                    call_id: call_id.to_string(),
                },
            }))
            .await?;
        match event {
            VendorEvent::SessionStartResult(ev) => Ok(ev.response.context),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ToolResult(_)
            | VendorEvent::ScanResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a session start with the wrong event".to_string(),
            )),
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
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::runtime::{BashInput, ToolError, ToolOutput};
    use horsie_models::vendor::{
        VendorAgentCapabilities, VendorInboundMessage, VendorOutboundMessage, VendorRegistered,
        VendorToolResult,
    };
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn boot_agent(answer_calls: bool, stdout: &'static str) -> Arc<VendorLink> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        tokio::spawn(async move {
            let boot = VendorOutboundMessage {
                request_id: "boot".to_string(),
                event: VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorAgentCapabilities {
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
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                let call_id = match &inbound.command {
                    VendorCommand::ToolCall(c) => c.call.call_id.clone(),
                    other => panic!("unexpected command {other:?}"),
                };
                let reply = VendorOutboundMessage {
                    request_id: inbound.request_id,
                    event: VendorEvent::ToolResult(VendorToolResult {
                        runtime_id: "rt-1".to_string(),
                        call_id,
                        result: ToolResult::Ok(ToolOutput {
                            stdout: stdout.to_string(),
                            stderr: String::new(),
                            exit_code: 0,
                        }),
                    }),
                };
                agent
                    .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .unwrap();
            }
        });
        VendorLink::start(server).await.unwrap()
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
        let transport = VendorRuntimeTransport::new(link, "rt-1".to_string());
        let result = transport.invoke("call-1", bash()).await.expect("tool call");
        match result {
            ToolResult::Ok(out) => assert_eq!(out.stdout, "hello-from-agent"),
            ToolResult::Err(ToolError { reason }) => panic!("expected success, got {reason}"),
        }
    }

    #[tokio::test]
    async fn invoke_reports_disconnected_so_runtime_client_latches() {
        let link = boot_agent(false, "").await;
        let transport = VendorRuntimeTransport::new(link, "rt-1".to_string());
        let err = transport
            .invoke("call-1", bash())
            .await
            .expect_err("a dead link must fail the call");
        assert!(
            matches!(err, TransportError::Disconnected),
            "RuntimeClient only latches on Disconnected, got {err:?}"
        );
    }
}
