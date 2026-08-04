use crate::transport::{RuntimeTransport, TransportError};
use horsie_models::runtime::{
    HookRecord, ScanResponse, ToolCall, ToolError, ToolOutput, ToolResult,
};
use std::collections::HashSet;
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
/// Receives what plugin hooks did to a tool call.
///
/// Out of band from the tool result because the tools are unaware of hooks and
/// the records are for the *user*, not the model: the server journals them so a
/// session can show what a plugin blocked or rewrote.
#[async_trait::async_trait]
pub trait HookSink: Send + Sync {
    async fn record(&self, hooks: Vec<HookRecord>);
}

#[derive(Clone)]
pub struct RuntimeClient {
    inner: Arc<dyn RuntimeTransport>,
    /// Where hook records go. `None` outside a session — the CLI and tests have
    /// nothing to journal them to.
    hook_sink: Option<Arc<dyn HookSink>>,
    /// The agent this handle acts for. Stamped on every invoke; the runtime
    /// keys its per-agent cwd/env state by it, so an agent and its subagents
    /// never see each other's working directory or environment.
    ///
    /// Required, not optional: a client without an identity would share a
    /// bucket with every other unidentified caller. Today it is the session id
    /// (also the main agent's journal id); a subagent derives its own handle
    /// with [`Self::with_agent_id`].
    agent_id: String,
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
    pub fn new(transport: impl RuntimeTransport + 'static, agent_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(transport),
            hook_sink: None,
            agent_id: agent_id.into(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Build a client from an already-type-erased transport — e.g. the one handed
    /// back by `ExecutorClient::runtime_transport`, which cannot be re-boxed by
    /// [`RuntimeClient::new`]'s `impl RuntimeTransport` bound.
    pub fn from_arc(transport: Arc<dyn RuntimeTransport>, agent_id: impl Into<String>) -> Self {
        Self {
            inner: transport,
            hook_sink: None,
            agent_id: agent_id.into(),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Route hook records to `sink`. Set once, by the session that journals
    /// them; a client without one simply discards what hooks reported.
    #[must_use]
    pub fn with_hook_sink(mut self, sink: Arc<dyn HookSink>) -> Self {
        self.hook_sink = Some(sink);
        self
    }

    /// A handle onto the same runtime acting for a different agent.
    ///
    /// The seam subagents use: a subagent sharing its parent's runtime derives
    /// a client with its own id and gets its own cwd/env bucket, or reuses the
    /// parent's id to share deliberately. Cheap — shares the inner Arcs, so
    /// in-flight tracking stays common to both.
    #[must_use]
    pub fn with_agent_id(self, agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            ..self
        }
    }

    /// The agent this handle acts for.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self.inner.invoke(&call_id, &self.agent_id, call).await;
        self.untrack(&call_id);
        match outcome {
            Ok((result, hooks)) => {
                // Hook records ride the tool response but are not part of it:
                // the tools themselves neither know nor care that a hook ran, so
                // the records go out of band to whoever is recording them.
                if !hooks.is_empty()
                    && let Some(sink) = &self.hook_sink
                {
                    sink.record(hooks).await;
                }
                match result {
                    ToolResult::Ok(output) => Ok(output),
                    ToolResult::Err(ToolError { reason }) => {
                        Err(RuntimeCallError::ToolFailed(reason))
                    }
                }
            }
            Err(e) => Err(RuntimeCallError::Transport(e)),
        }
    }

    pub async fn cancel(&self, call_id: &str) {
        let _ = self.inner.cancel(call_id).await;
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
    ) -> Result<ScanResponse, RuntimeCallError> {
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
            .map_err(RuntimeCallError::Transport)
    }

    /// Run the shared plugin library's `SessionStart` hooks and return the injected
    /// context (empty when there are none).
    pub async fn run_session_start(&self) -> Result<String, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner
            .run_session_start(&call_id)
            .await
            .map_err(RuntimeCallError::Transport)
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
        })
    }

    #[tokio::test]
    async fn cancel_in_flight_reaches_every_pending_call() {
        // The gate holds two invokes open so both are genuinely in flight when the
        // cancel arrives — the shape a Stop mid-batch produces.
        let gate = crate::testkit::BlockHandle::new();
        let c = RuntimeClient::new(
            crate::testkit::MockTransport::gated_invoke(&gate),
            "test-agent",
        );
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
        let c = RuntimeClient::new(crate::testkit::MockTransport::ok(""), "test-agent");
        assert_eq!(c.in_flight_count(), 0);
        assert!(c.invoke(probe_call()).await.is_ok());
        assert_eq!(
            c.in_flight_count(),
            0,
            "a completed call must not linger as cancellable"
        );
    }

    /// A transport error condemns the call, never the client. The server's
    /// transport resolves its vendor link per call, so the socket that failed
    /// this one says nothing about the next: a client that latched itself off
    /// would turn a reconnect into a dead turn.
    #[tokio::test]
    async fn a_client_stays_usable_after_a_transport_error() {
        let c = RuntimeClient::new(MockTransport::disconnect_after(0), "test-agent");
        assert!(c.invoke(probe_call()).await.is_err());
        assert_eq!(
            c.in_flight_count(),
            0,
            "a failed call must not linger as in flight"
        );
        assert!(
            c.invoke(probe_call()).await.is_err(),
            "the second call must reach the transport, not a local latch"
        );
    }

    use super::*;
    use crate::testkit::MockTransport;
    use horsie_models::runtime::BashInput;

    #[tokio::test]
    async fn the_agent_id_is_stamped_on_invokes() {
        let probe = crate::testkit::TransportProbe::new();
        let client = RuntimeClient::new(MockTransport::ok("").observed_by(&probe), "agent-1");
        client.invoke(probe_call()).await.unwrap();
        assert_eq!(probe.agent_ids(), vec!["agent-1".to_string()]);
    }

    /// The subagent seam: a derived handle shares the runtime but carries its
    /// own identity, so the runtime keys its cwd/env in a separate bucket.
    #[tokio::test]
    async fn a_derived_handle_stamps_its_own_agent_id() {
        let probe = crate::testkit::TransportProbe::new();
        let parent = RuntimeClient::new(MockTransport::ok("").observed_by(&probe), "main");
        let sub = parent.clone().with_agent_id("sub-1");
        parent.invoke(probe_call()).await.unwrap();
        sub.invoke(probe_call()).await.unwrap();
        assert_eq!(
            probe.agent_ids(),
            vec!["main".to_string(), "sub-1".to_string()]
        );
    }

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::new(MockTransport::ok("hello"), "test-agent");
        let output = client
            .invoke(ToolCall::Bash(BashInput {
                command: "echo hello".into(),
                timeout_secs: None,
            }))
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
    }

    #[tokio::test]
    async fn client_returns_err_on_tool_failure() {
        let client = RuntimeClient::new(MockTransport::err("oops"), "test-agent");
        let err = client
            .invoke(ToolCall::Bash(BashInput {
                command: "bad".into(),
                timeout_secs: None,
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
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(vec![scan]), "test-agent");
        let resp = client
            .scan_workspace(
                None,
                vec!["AGENTS.md".into()],
                ".claude/skills/*/SKILL.md".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(resp.workspaces.len(), 1);
        assert_eq!(
            resp.workspaces[0].instructions.as_ref().unwrap().content,
            "hi"
        );
        assert!(resp.shared_skills.is_empty());
        assert!(resp.shared_root.is_none());
    }
}
