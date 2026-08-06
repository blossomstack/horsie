use async_trait::async_trait;
use horsie_models::hooks::HookRecord;
use horsie_models::runtime::{
    CancelCallRequest, McpCredential, McpDiscoverRequest, McpDiscoverResponse, McpInvokeRequest,
    McpInvokeResponse, RunHooksRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ScanRequest,
    ScanResponse, ServerHookEvent, ToolCall, ToolCallRequest, ToolResult,
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
    ) -> Result<(ToolResult, Vec<HookRecord>), TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::ToolCall(ToolCallRequest {
                call_id: call_id.to_string(),
                agent_id: agent_id.to_string(),
                call,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::ToolCallResponse(resp) => Ok((resp.result, resp.hooks)),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::HookRecords(_)
            | RuntimeOutboundMessage::McpTools(_)
            | RuntimeOutboundMessage::McpResult(_) => Err(wrong_reply("a tool call")),
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
            | RuntimeOutboundMessage::HookRecords(_)
            | RuntimeOutboundMessage::McpTools(_)
            | RuntimeOutboundMessage::McpResult(_) => Err(wrong_reply("a workspace scan")),
        }
    }

    /// Run every hook matching a server-initiated `event` in the sandbox.
    ///
    /// The general form of what used to be a `SessionStart`-only RPC. Injected
    /// context is derived from the records by the caller rather than carried
    /// beside them, so no event is reported specially.
    async fn run_hooks(
        &self,
        call_id: &str,
        event: &ServerHookEvent,
    ) -> Result<Vec<HookRecord>, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::RunHooks(RunHooksRequest {
                call_id: call_id.to_string(),
                event: event.clone(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::HookRecords(resp) => Ok(resp.records),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::McpTools(_)
            | RuntimeOutboundMessage::McpResult(_) => Err(wrong_reply("a hook run")),
        }
    }

    /// List every tool the loaded plugins' MCP servers offer.
    ///
    /// One request for all of them: a session wants the whole list or none, and
    /// a server that cannot start contributes nothing rather than failing.
    async fn mcp_discover(
        &self,
        call_id: &str,
        credentials: Vec<McpCredential>,
    ) -> Result<McpDiscoverResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::McpDiscover(McpDiscoverRequest {
                call_id: call_id.to_string(),
                credentials,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::McpTools(resp) => Ok(resp),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::HookRecords(_)
            | RuntimeOutboundMessage::McpResult(_) => Err(wrong_reply("MCP discovery")),
        }
    }

    /// Call one namespaced plugin MCP tool.
    async fn mcp_invoke(
        &self,
        call_id: &str,
        tool: &str,
        arguments: String,
        credentials: Vec<McpCredential>,
    ) -> Result<McpInvokeResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::McpInvoke(McpInvokeRequest {
                call_id: call_id.to_string(),
                tool: tool.to_string(),
                arguments,
                credentials,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::McpResult(resp) => Ok(resp),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::HookRecords(_)
            | RuntimeOutboundMessage::McpTools(_) => Err(wrong_reply("an MCP tool call")),
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
        RuntimeInboundMessage::RunHooks(req) => &req.call_id,
        RuntimeInboundMessage::McpDiscover(req) => &req.call_id,
        RuntimeInboundMessage::McpInvoke(req) => &req.call_id,
    }
}

/// The request this reply answers, or `None` for the handshake messages a runtime
/// sends unprompted.
#[must_use]
pub fn outbound_call_id(message: &RuntimeOutboundMessage) -> Option<&str> {
    match message {
        RuntimeOutboundMessage::ToolCallResponse(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::ScanResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::HookRecords(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::McpTools(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::McpResult(resp) => Some(&resp.call_id),
        RuntimeOutboundMessage::Ready(_)
        | RuntimeOutboundMessage::Provisioning(_)
        | RuntimeOutboundMessage::ProvisionFailed(_) => None,
    }
}
