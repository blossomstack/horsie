use async_trait::async_trait;
use horsie_models::runtime::{
    CancelCallRequest, HookManifestRequest, HookManifestResponse, HookOutcomeWire,
    RunHookRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ScanRequest, ScanResponse,
    SessionStartRequest, ToolCall, ToolCallRequest, ToolResult,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("disconnected")]
    Disconnected,
}

/// A pipe to one runtime, carrying `runtime.fl` messages verbatim.
///
/// Implementors provide the two primitives — [`relay`](Self::relay) for a request
/// that draws a reply and [`send_oneway`](Self::send_oneway) for one that does not.
/// Every typed operation below is a default built on them, so adding a runtime
/// capability means adding a message to `runtime.fl` and a default method here:
/// no implementor changes, and nothing in between (the vendor link, the vendor
/// agent) learns a new variant to forward.
#[async_trait]
pub trait RuntimeTransport: Send + Sync {
    /// Send a message and await the runtime's correlated reply.
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError>;

    /// Send a message the runtime never answers. `CancelCall` is the only one
    /// today, and a caller that is tearing a turn down must not block on it.
    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError>;

    /// `agent_id` keys the runtime's per-agent cwd/env state. It is a required
    /// parameter rather than transport state so the single place that builds a
    /// [`ToolCallRequest`] cannot omit it — an unkeyed call would share mutable
    /// state with every other unkeyed caller.
    async fn invoke(
        &self,
        call_id: &str,
        agent_id: &str,
        call: ToolCall,
    ) -> Result<ToolResult, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::ToolCall(ToolCallRequest {
                call_id: call_id.to_string(),
                agent_id: agent_id.to_string(),
                call,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::ToolCallResponse(resp) => Ok(resp.result),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::HookManifestResult(_)
            | RuntimeOutboundMessage::RunHookResult(_) => Err(wrong_reply("a tool call")),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        self.send_oneway(RuntimeInboundMessage::CancelCall(CancelCallRequest {
            call_id: call_id.to_string(),
        }))
        .await
    }

    /// Scan the selected workspaces (`workspace`: `None` = all, `Some(name)` = one),
    /// reading the first existing instruction candidate (in order) and every file
    /// matching `skills_glob` per root, returning raw contents. Name→path resolution
    /// happens runtime-side against its workspace registry. When `include_shared` is
    /// set, the shared plugin library's skills come back alongside its absolute root.
    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<ScanResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::ScanWorkspace(ScanRequest {
                call_id: call_id.to_string(),
                workspace,
                instruction_candidates,
                skills_glob,
                include_shared,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::ScanResult(resp) => Ok(resp),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::HookManifestResult(_)
            | RuntimeOutboundMessage::RunHookResult(_) => Err(wrong_reply("a workspace scan")),
        }
    }

    /// Run the shared plugin library's `SessionStart` hooks in the sandbox and return
    /// their concatenated injected context (empty when there are none).
    async fn run_session_start(&self, call_id: &str) -> Result<String, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::SessionStart(SessionStartRequest {
                call_id: call_id.to_string(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::SessionStartResult(resp) => Ok(resp.context),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::HookManifestResult(_)
            | RuntimeOutboundMessage::RunHookResult(_) => Err(wrong_reply("a session start")),
        }
    }

    /// What hooks the session's plugins declare.
    ///
    /// A runtime that does not implement this message answers with an error,
    /// which the caller reads as "this runtime has no hook support" rather than
    /// as a failure — the protocol carries no version, so this call is also the
    /// negotiation.
    async fn hook_manifest(&self, call_id: &str) -> Result<HookManifestResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::HookManifest(HookManifestRequest {
                call_id: call_id.to_string(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::HookManifestResult(resp) => Ok(resp),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::RunHookResult(_) => Err(wrong_reply("a hook manifest")),
        }
    }

    /// Run every hook matching `event` and return their merged outcome.
    async fn run_hook(
        &self,
        call_id: &str,
        event: &str,
        payload: &str,
    ) -> Result<HookOutcomeWire, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::RunHook(RunHookRequest {
                call_id: call_id.to_string(),
                event: event.to_string(),
                payload: payload.to_string(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::RunHookResult(resp) => Ok(resp.outcome),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::HookManifestResult(_) => Err(wrong_reply("a hook run")),
        }
    }
}

fn wrong_reply(what: &str) -> TransportError {
    TransportError::SendFailed(format!(
        "the runtime answered {what} with the wrong message"
    ))
}

/// The correlation id a runtime echoes back on its reply.
///
/// Every inbound message carries one, including `CancelCall` — where it names the
/// call being abandoned rather than this message, and so must never be registered
/// as a waiter.
#[must_use]
pub fn inbound_call_id(message: &RuntimeInboundMessage) -> &str {
    match message {
        RuntimeInboundMessage::ToolCall(req) => &req.call_id,
        RuntimeInboundMessage::CancelCall(req) => &req.call_id,
        RuntimeInboundMessage::ScanWorkspace(req) => &req.call_id,
        RuntimeInboundMessage::SessionStart(req) => &req.call_id,
        RuntimeInboundMessage::HookManifest(req) => &req.call_id,
        RuntimeInboundMessage::RunHook(req) => &req.call_id,
    }
}

/// The request this reply answers, or `None` for the handshake messages a runtime
/// sends unprompted.
#[must_use]
pub fn outbound_call_id(message: &RuntimeOutboundMessage) -> Option<&str> {
    match message {
        RuntimeOutboundMessage::ToolCallResponse(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::ScanResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::SessionStartResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::HookManifestResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::RunHookResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::Ready(_)
        | RuntimeOutboundMessage::Provisioning(_)
        | RuntimeOutboundMessage::ProvisionFailed(_) => None,
    }
}
