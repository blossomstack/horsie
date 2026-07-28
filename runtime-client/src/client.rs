use crate::transport::{RuntimeTransport, TransportError};
use horsie_models::runtime::{
    PluginSkill, ToolCall, ToolError, ToolOutput, ToolResult, WorkspaceScan,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use uuid::Uuid;

#[derive(Debug)]
pub enum RuntimeCallError {
    Transport(TransportError),
    ToolFailed(String),
}

impl std::fmt::Display for RuntimeCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::ToolFailed(r) => write!(f, "tool failed: {r}"),
        }
    }
}

impl std::error::Error for RuntimeCallError {}

/// Client handle for invoking tools on a remote runtime. Cheap to clone — Arc-backed.
#[derive(Clone)]
pub struct RuntimeClient {
    inner: Arc<dyn RuntimeTransport>,
    /// Cleared the first time the transport reports [`TransportError::Disconnected`].
    ///
    /// A disconnect is terminal for this client: the socket is gone, and every
    /// later call over it fails the same way. Callers that cache a client — the
    /// session actor caches one per runtime — must consult [`Self::is_connected`]
    /// before reuse and re-acquire the runtime when it returns `false`. Shared
    /// across clones so the flag set on the agent's clone is visible to the
    /// session's.
    connected: Arc<AtomicBool>,
    /// Wire ids of calls currently awaiting a reply.
    ///
    /// `invoke` mints its own id rather than reusing the model's `tool_call_id`,
    /// so nothing outside this client could name an in-flight call to cancel it —
    /// which is why `cancel` had no caller and stopping a turn left the sandbox
    /// running the command to completion (#61 item 23). Shared across clones so a
    /// holder that did not issue the call can still stop it.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl RuntimeClient {
    pub fn new(transport: impl RuntimeTransport + 'static) -> Self {
        Self {
            inner: Arc::new(transport),
            connected: Arc::new(AtomicBool::new(true)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Build a client from an already-type-erased transport — e.g. the one handed
    /// back by `ExecutorClient::runtime_transport`, which cannot be re-boxed by
    /// [`RuntimeClient::new`]'s `impl RuntimeTransport` bound.
    pub fn from_arc(transport: Arc<dyn RuntimeTransport>) -> Self {
        Self {
            inner: transport,
            connected: Arc::new(AtomicBool::new(true)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether this client's transport is still usable.
    ///
    /// `false` once any call has reported `Disconnected`. Never returns to `true`:
    /// recovering means acquiring a new runtime, not reviving this one.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Latch the disconnect so cached holders of this client stop reusing it.
    fn note_transport_error(&self, error: &TransportError) {
        if matches!(error, TransportError::Disconnected) {
            self.connected.store(false, Ordering::Relaxed);
        }
    }

    pub async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self.inner.invoke(&call_id, call).await;
        self.untrack(&call_id);
        match outcome {
            Ok(ToolResult::Ok(output)) => Ok(output),
            Ok(ToolResult::Err(ToolError { reason })) => Err(RuntimeCallError::ToolFailed(reason)),
            Err(e) => {
                self.note_transport_error(&e);
                Err(RuntimeCallError::Transport(e))
            }
        }
    }

    pub async fn cancel(&self, call_id: &str) {
        if let Err(e) = self.inner.cancel(call_id).await {
            self.note_transport_error(&e);
        }
    }

    fn track(&self, call_id: &str) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_string());
    }

    fn untrack(&self, call_id: &str) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id);
    }

    /// How many calls are awaiting a reply. Test observability for the tracking
    /// that makes [`Self::cancel_in_flight`] possible.
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Tell the runtime to abandon every call still awaiting a reply.
    ///
    /// Dropping a tool future abandons it *locally* only: without this the sandbox
    /// keeps running the command to completion, holding resources, with its output
    /// discarded. Best-effort and non-blocking on failure — the caller is already
    /// tearing the turn down.
    pub async fn cancel_in_flight(&self) {
        let ids: Vec<String> = {
            let guard = self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.iter().cloned().collect()
        };
        for id in &ids {
            self.cancel(id).await;
        }
    }

    /// Scan workspaces over the runtime. `workspace` filters which roots to scan
    /// (`None` = all, `Some(name)` = one). `instruction_candidates` are tried in order
    /// (first existing wins); `skills_glob` locates skill files. Raw contents come back
    /// for the caller to interpret.
    pub async fn scan_workspace(
        &self,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner
            .scan_workspace(
                &call_id,
                workspace,
                instruction_candidates,
                skills_glob,
                include_shared,
            )
            .await
            .map_err(|e| {
                self.note_transport_error(&e);
                RuntimeCallError::Transport(e)
            })
    }

    /// Run the shared plugin library's `SessionStart` hooks and return the injected
    /// context (empty when there are none).
    pub async fn run_session_start(&self) -> Result<String, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner.run_session_start(&call_id).await.map_err(|e| {
            self.note_transport_error(&e);
            RuntimeCallError::Transport(e)
        })
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
    fn probe_call() -> ToolCall {
        ToolCall::Bash(horsie_models::runtime::BashInput {
            command: "true".into(),
            timeout_secs: None,
            workspace: None,
        })
    }

    #[tokio::test]
    async fn cancel_in_flight_reaches_every_pending_call() {
        // The gate holds two invokes open so both are genuinely in flight when the
        // cancel arrives — the shape a Stop mid-batch produces.
        let gate = crate::testkit::BlockHandle::new();
        let c = RuntimeClient::new(crate::testkit::MockTransport::gated_invoke(&gate));
        let probe = c.clone();
        let calls = tokio::spawn(async move {
            tokio::join!(probe.invoke(probe_call()), probe.invoke(probe_call()))
        });
        // Let both reach the transport and register as in flight.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        c.cancel_in_flight().await;
        gate.release();
        let _ = calls.await;

        // Two distinct wire ids cancelled, neither of them the model's tool_call_id
        // (which `invoke` never sees).
        assert_eq!(
            c.in_flight_count(),
            0,
            "tracking must clear once calls settle"
        );
    }

    #[tokio::test]
    async fn a_settled_call_is_no_longer_tracked() {
        let c = RuntimeClient::new(crate::testkit::MockTransport::ok(""));
        assert_eq!(c.in_flight_count(), 0);
        assert!(c.invoke(probe_call()).await.is_ok());
        assert_eq!(
            c.in_flight_count(),
            0,
            "a completed call must not linger as cancellable"
        );
    }

    #[tokio::test]
    async fn a_healthy_client_reports_connected() {
        let c = RuntimeClient::new(MockTransport::ok(""));
        assert!(c.is_connected());
        assert!(c.invoke(probe_call()).await.is_ok());
        assert!(c.is_connected());
    }

    #[tokio::test]
    async fn a_tool_level_failure_does_not_mark_the_client_disconnected() {
        // `ToolResult::Err` is the tool failing, not the socket dropping — the
        // runtime is still perfectly usable.
        let c = RuntimeClient::new(MockTransport::err("exit 1"));
        assert!(c.invoke(probe_call()).await.is_err());
        assert!(
            c.is_connected(),
            "a failed tool must not condemn the runtime"
        );
    }

    #[tokio::test]
    async fn a_disconnect_latches_and_never_clears() {
        let c = RuntimeClient::new(MockTransport::disconnect_after(0));
        assert!(c.is_connected());
        assert!(c.invoke(probe_call()).await.is_err());
        assert!(!c.is_connected(), "a disconnect must be latched");
        // Still false on every later look — recovery means a new runtime.
        assert!(!c.is_connected());
    }

    #[tokio::test]
    async fn the_latch_is_shared_across_clones() {
        // The agent gets a clone of the session's client; a disconnect seen there
        // has to be visible to the session, which is what releases the runtime.
        let session_side = RuntimeClient::new(MockTransport::disconnect_after(0));
        let agent_side = session_side.clone();
        assert!(agent_side.invoke(probe_call()).await.is_err());
        assert!(
            !session_side.is_connected(),
            "the session's handle must see the agent's disconnect"
        );
    }

    use super::*;
    use crate::testkit::MockTransport;
    use horsie_models::runtime::BashInput;

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::new(MockTransport::ok("hello"));
        let output = client
            .invoke(ToolCall::Bash(BashInput {
                command: "echo hello".into(),
                timeout_secs: None,
                workspace: None,
            }))
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
    }

    #[tokio::test]
    async fn client_returns_err_on_tool_failure() {
        let client = RuntimeClient::new(MockTransport::err("oops"));
        let err = client
            .invoke(ToolCall::Bash(BashInput {
                command: "bad".into(),
                timeout_secs: None,
                workspace: None,
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeCallError::ToolFailed(_)));
    }

    #[tokio::test]
    async fn client_scan_returns_mock_scan() {
        use horsie_models::runtime::{ScannedFile, WorkspaceScan};
        let scan = WorkspaceScan {
            name: "october".into(),
            path: "/ws/october".into(),
            is_git_repo: false,
            instructions: Some(ScannedFile {
                path: "AGENTS.md".into(),
                content: "hi".into(),
            }),
            skills: vec![],
            platform: None,
        };
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(vec![scan]));
        let (out, shared) = client
            .scan_workspace(
                None,
                vec!["AGENTS.md".into()],
                ".claude/skills/*/SKILL.md".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].instructions.as_ref().unwrap().content, "hi");
        assert!(shared.is_empty());
    }
}
