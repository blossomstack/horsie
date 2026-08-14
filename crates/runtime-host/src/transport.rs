use async_trait::async_trait;
use horsie_models::hooks::HookRecord;
use horsie_models::runtime::{
    CancelCallRequest, McpDiscoverRequest, McpDiscoverResponse, McpInvokeRequest, RunHooksRequest,
    RuntimeInboundMessage, RuntimeOutboundMessage, ScanRequest, ScanResponse, ServerHookEvent,
    ToolCall, ToolCallRequest, ToolResult,
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

    /// Resolves once this runtime can no longer be reached.
    ///
    /// The unifying signal for a runtime going away, whatever noticed first: a
    /// vendor link reporting a state change, a WebSocket closing, or a
    /// substrate that reported a dead machine.
    ///
    /// Defaulted to *never*, which is the honest answer for a transport that
    /// tracks nothing. Saying "I cannot tell you" costs a parked task; saying
    /// "it is dead" costs a live runtime, since whoever holds the transport
    /// drops it. A transport with a real signal overrides this, and one backed
    /// by a `watch` channel should do so through [`closed_when`].
    async fn closed(&self) {
        closed_when(None).await
    }

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
    async fn mcp_discover(&self, call_id: &str) -> Result<McpDiscoverResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::McpDiscover(McpDiscoverRequest {
                call_id: call_id.to_string(),
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
    ) -> Result<ToolResult, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::McpInvoke(McpInvokeRequest {
                call_id: call_id.to_string(),
                tool: tool.to_string(),
                arguments,
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::McpResult(resp) => Ok(resp.result),
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

/// The body of a [`RuntimeTransport::closed`] backed by a `watch` channel.
///
/// Free-standing so the two rules it encodes stay testable without a transport
/// to hang them on, and so every transport that has such a channel encodes them
/// the same way:
///
/// - the value is checked **before** waiting, because a flip that happened
///   before this clone must still be answered rather than waited on forever;
/// - a dropped sender means nobody is left to report a closure, which is not
///   the same as a closure, so it waits rather than resolving.
pub async fn closed_when(watched: Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(mut rx) = watched else {
        return std::future::pending().await;
    };
    if *rx.borrow() {
        return;
    }
    if rx.wait_for(|closed| *closed).await.is_err() {
        std::future::pending::<()>().await;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::MockTransport;

    /// The bug a dropped sender hid. A vendor whose registry reports liveness
    /// by presence has no flag to flip, and the stand-in was a `watch` channel
    /// whose sender was dropped on the spot — so `wait_for` errored at once and
    /// `closed()` reported every live runtime dead the instant it was asked.
    #[tokio::test]
    async fn a_dropped_watcher_is_not_a_closed_runtime() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        drop(tx);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), closed_when(Some(rx)))
                .await
                .is_err(),
            "nobody left to report a closure is not the same as a closure"
        );
    }

    #[tokio::test]
    async fn a_closure_resolves_it() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let waiting = tokio::spawn(closed_when(Some(rx)));
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("closed must resolve once the connection is marked dead")
            .unwrap();
    }

    #[tokio::test]
    async fn a_closure_that_already_happened_resolves_immediately() {
        // Checking the value only after subscribing would hang forever on a
        // flip that landed before this receiver was cloned.
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(200), closed_when(Some(rx)))
            .await
            .expect("closed must not wait for a flip that already happened");
    }

    /// A transport that tracks nothing must never claim its runtime died.
    ///
    /// Saying "I cannot tell you" costs a parked task; saying "it is dead"
    /// costs a live runtime, because whoever holds the transport drops it.
    #[tokio::test]
    async fn a_transport_that_tracks_nothing_never_reports_a_closure() {
        let transport = MockTransport::ok("");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), transport.closed())
                .await
                .is_err(),
            "an untracked transport must not resolve `closed`"
        );
    }
}
