use crate::transport::{RuntimeTransport, TransportError};
use horsie_models::hooks::HookRecord;
use horsie_models::runtime::{
    ScanResponse, ServerHookEvent, ToolCall, ToolError, ToolOutput, ToolResult,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
// Still minted for the calls that have no model tool_call_id to borrow —
// `scan_workspace` and `run_hooks` are server-initiated, not tool calls.
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
    /// Ids of calls currently awaiting a reply — the model's own `tool_call_id`s.
    ///
    /// `invoke` used to mint a private id here, so nothing outside this client
    /// could name an in-flight call to cancel it; that is why `cancel` had no
    /// caller and stopping a turn left the sandbox running the command to
    /// completion (#61 item 23). Shared across clones so a holder that did not
    /// issue the call can still stop it.
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

    /// The same handle with its hook sink detached, for a caller that journals
    /// the records itself.
    ///
    /// The one caller is the agent's pre-run seam: its records must be folded
    /// into state *before* the turn reads its prompt, so they travel back on the
    /// return value and go through the agent's own mailbox. Left on the sink,
    /// they would take the longer agent → session → agent route and could land
    /// after the turn they are supposed to precede — as well as being journaled
    /// twice.
    #[must_use]
    pub fn without_hook_sink(mut self) -> Self {
        self.hook_sink = None;
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

    /// `call_id` is the model's own tool-call id, not one minted here. One id
    /// space for three consumers — cancellation, the runtime, and the hook
    /// records that must join back to this call in the transcript — because two
    /// spaces cannot be correlated after the fact under parallel tool use.
    pub async fn invoke(
        &self,
        call_id: &str,
        call: ToolCall,
    ) -> Result<ToolOutput, RuntimeCallError> {
        self.track(call_id);
        let outcome = self.inner.invoke(call_id, &self.agent_id, call).await;
        self.untrack(call_id);
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

    /// Run every hook matching a server-initiated event.
    ///
    /// Mints its own `call_id`: unlike a tool hook this is not correlated to
    /// anything the model said, so there is no id to borrow.
    ///
    /// Records go to the same [`HookSink`] tool records take, so a
    /// server-initiated hook reaches the transcript by one route rather than a
    /// second one. They are also returned, because the caller has to act on
    /// them — inject the context, or honour a block.
    pub async fn run_hooks(
        &self,
        event: ServerHookEvent,
    ) -> Result<Vec<HookRecord>, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        let records = self
            .inner
            .run_hooks(&call_id, &event)
            .await
            .map_err(RuntimeCallError::Transport)?;
        if let Some(sink) = &self.hook_sink
            && !records.is_empty()
        {
            sink.record(records.clone()).await;
        }
        Ok(records)
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
            tokio::join!(
                probe.invoke("tc1", probe_call()),
                probe.invoke("tc2", probe_call())
            )
        });
        // Let both reach the transport and register as in flight.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        c.cancel_in_flight().await;
        gate.release();
        let _ = calls.await;

        // Both of the model's tool_call_ids were tracked and cancelled: `invoke`
        // now uses that id directly rather than minting a wire id of its own.
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
        assert!(c.invoke("tc1", probe_call()).await.is_ok());
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
        assert!(c.invoke("tc1", probe_call()).await.is_err());
        assert_eq!(
            c.in_flight_count(),
            0,
            "a failed call must not linger as in flight"
        );
        assert!(
            c.invoke("tc2", probe_call()).await.is_err(),
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
        client.invoke("tc1", probe_call()).await.unwrap();
        assert_eq!(probe.agent_ids(), vec!["agent-1".to_string()]);
    }

    /// The model's tool_call_id reaches the runtime verbatim. Everything
    /// downstream depends on it: cancellation names a call by it, and a hook
    /// record carries it back so the transcript can attach the record to the
    /// tool result. A privately minted id would satisfy the first and silently
    /// break the second.
    #[tokio::test]
    async fn the_models_call_id_reaches_the_runtime_unchanged() {
        let probe = crate::testkit::TransportProbe::new();
        let client = RuntimeClient::new(MockTransport::ok("").observed_by(&probe), "agent-1");
        client.invoke("toolu_abc123", probe_call()).await.unwrap();
        client.invoke("toolu_def456", probe_call()).await.unwrap();
        assert_eq!(
            probe.call_ids(),
            vec!["toolu_abc123".to_string(), "toolu_def456".to_string()]
        );
    }

    /// Hook records must reach their sink *before* `invoke` returns.
    ///
    /// This is what orders a hook entry ahead of the tool result it describes:
    /// the agent loop journals `ToolComplete` only after `execute` — and so
    /// `invoke` — has returned, so a sink that has already been told puts its
    /// record on the actor's mailbox first. Let the sink be told afterwards and
    /// the transcript shows a hook explaining a call the reader already scrolled
    /// past.
    #[tokio::test]
    async fn hook_records_reach_the_sink_before_invoke_returns() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct Latch(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl HookSink for Latch {
            async fn record(&self, hooks: Vec<HookRecord>) {
                assert_eq!(hooks.len(), 1, "the response's records, verbatim");
                self.0.store(true, AtomicOrdering::SeqCst);
            }
        }

        let record = HookRecord {
            plugin: "guard".into(),
            duration_ms: 1,
            halt: None,
            action: horsie_models::hooks::HookAction::PreToolUse(
                horsie_models::hooks::PreToolUseRecord {
                    call: horsie_models::hooks::ToolScope {
                        tool: "bash".into(),
                        tool_call_id: "tc1".into(),
                    },
                    system_message: None,
                    outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                        horsie_models::hooks::HookDenied { reason: None },
                    ),
                },
            ),
        };
        let told = Arc::new(AtomicBool::new(false));
        let client = RuntimeClient::new(
            MockTransport::ok("done").with_hooks(vec![record]),
            "agent-1",
        )
        .with_hook_sink(Arc::new(Latch(told.clone())));

        client.invoke("tc1", probe_call()).await.unwrap();
        assert!(
            told.load(AtomicOrdering::SeqCst),
            "the sink must be told before invoke hands the result back"
        );
    }

    /// The subagent seam: a derived handle shares the runtime but carries its
    /// own identity, so the runtime keys its cwd/env in a separate bucket.
    #[tokio::test]
    async fn a_derived_handle_stamps_its_own_agent_id() {
        let probe = crate::testkit::TransportProbe::new();
        let parent = RuntimeClient::new(MockTransport::ok("").observed_by(&probe), "main");
        let sub = parent.clone().with_agent_id("sub-1");
        parent.invoke("tc1", probe_call()).await.unwrap();
        sub.invoke("tc2", probe_call()).await.unwrap();
        assert_eq!(
            probe.agent_ids(),
            vec!["main".to_string(), "sub-1".to_string()]
        );
    }

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::new(MockTransport::ok("hello"), "test-agent");
        let output = client
            .invoke(
                "tc1",
                ToolCall::Bash(BashInput {
                    command: "echo hello".into(),
                    timeout_secs: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
    }

    #[tokio::test]
    async fn client_returns_err_on_tool_failure() {
        let client = RuntimeClient::new(MockTransport::err("oops"), "test-agent");
        let err = client
            .invoke(
                "tc1",
                ToolCall::Bash(BashInput {
                    command: "bad".into(),
                    timeout_secs: None,
                }),
            )
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
