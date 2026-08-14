use crate::in_flight::InFlight;
use crate::transport::{RuntimeTransport, TransportError};
use horsie_models::executor::ProvisionStep;
use horsie_models::hooks::HookRecord;
use horsie_models::runtime::{
    BundleRef, ProvisionError, ProvisionResult, ScanResponse, ServerHookEvent, ToolCall, ToolError,
    ToolOutput, ToolResult,
};
use std::sync::Arc;
// Minted for every server-initiated command — provisioning, scans, hooks and
// MCP discovery have no model `tool_call_id` to borrow. They are still tracked
// under it: the reconciler cancels any call the runtime reports that nothing
// here claims, so an untracked id is a live call with a target on its back.
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
    /// Calls awaiting a reply on this *runtime* — the model's own
    /// `tool_call_id`s, each against the agent that issued it.
    ///
    /// `invoke` used to mint a private id here, so nothing outside this client
    /// could name an in-flight call to cancel it; that is why `cancel` had no
    /// caller and stopping a turn left the sandbox running the command to
    /// completion (#61 item 23). Shared across clones so a holder that did not
    /// issue the call can still stop it — and now shared across every client for
    /// one runtime, because a reconciler needs the whole sandbox's calls rather
    /// than one agent's. See [`InFlight`].
    in_flight: Arc<InFlight>,
}

impl RuntimeClient {
    pub fn new(
        transport: impl RuntimeTransport + 'static,
        agent_id: impl Into<String>,
        in_flight: Arc<InFlight>,
    ) -> Self {
        Self {
            inner: Arc::new(transport),
            hook_sink: None,
            agent_id: agent_id.into(),
            in_flight,
        }
    }

    /// A client whose in-flight calls nothing else reconciles.
    ///
    /// For the callers that have no runtime manager and therefore no reconciler:
    /// the CLI, and tests. Named rather than defaulted so a production caller
    /// cannot reach it by accident — a client built this way is invisible to the
    /// reconciler for its runtime, whose diff would then read every call this
    /// client issued as an orphan and cancel it.
    pub fn detached(
        transport: impl RuntimeTransport + 'static,
        agent_id: impl Into<String>,
    ) -> Self {
        Self::new(transport, agent_id, Arc::new(InFlight::new()))
    }

    /// Build a client from an already-type-erased transport — e.g. the one handed
    /// back by `ExecutorClient::runtime_transport`, which cannot be re-boxed by
    /// [`RuntimeClient::new`]'s `impl RuntimeTransport` bound.
    pub fn from_arc(
        transport: Arc<dyn RuntimeTransport>,
        agent_id: impl Into<String>,
        in_flight: Arc<InFlight>,
    ) -> Self {
        Self {
            inner: transport,
            hook_sink: None,
            agent_id: agent_id.into(),
            in_flight,
        }
    }

    /// [`Self::from_arc`] for a caller with no reconciler — see [`Self::detached`].
    pub fn from_arc_detached(
        transport: Arc<dyn RuntimeTransport>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self::from_arc(transport, agent_id, Arc::new(InFlight::new()))
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

    /// Bring the workspaces to the state `steps` describes.
    ///
    /// Tracked like a tool call, and for the same two reasons. A clone of a
    /// large repository takes minutes, so the reconciler must see it as running
    /// — an untracked call the runtime reports is cancelled as an orphan. And a
    /// user hitting Stop during one must be able to abandon it.
    ///
    /// Mints its own `call_id`: unlike a tool call there is nothing the model
    /// said to borrow one from.
    pub async fn provision_workspace(
        &self,
        steps: Vec<ProvisionStep>,
    ) -> Result<(), RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self.inner.provision_workspace(&call_id, steps).await;
        self.untrack(&call_id);
        match outcome.map_err(RuntimeCallError::Transport)? {
            ProvisionResult::Ok(_) => Ok(()),
            ProvisionResult::Err(ProvisionError { reason }) => {
                Err(RuntimeCallError::ToolFailed(reason))
            }
        }
    }

    /// Install this agent's plugin tree, and answer with its root.
    ///
    /// Tracked like every other server-initiated command. Sent on every agent
    /// load: the runtime is the only party that knows what is already on its
    /// disk, so it absorbs the repeat rather than the server predicting it.
    pub async fn provision_agent(
        &self,
        bundles: Vec<BundleRef>,
    ) -> Result<String, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self
            .inner
            .provision_agent(&call_id, &self.agent_id, bundles)
            .await;
        self.untrack(&call_id);
        match outcome.map_err(RuntimeCallError::Transport)? {
            (ProvisionResult::Ok(_), root) => Ok(root),
            (ProvisionResult::Err(ProvisionError { reason }), _) => {
                Err(RuntimeCallError::ToolFailed(reason))
            }
        }
    }

    pub async fn cancel(&self, call_id: &str) {
        let _ = self.inner.cancel(call_id).await;
    }

    fn track(&self, call_id: &str) {
        self.in_flight.track(call_id, &self.agent_id);
    }

    fn untrack(&self, call_id: &str) {
        self.in_flight.untrack(call_id);
    }

    /// The map this client tracks into, for whoever reconciles this runtime.
    #[must_use]
    pub fn in_flight(&self) -> Arc<InFlight> {
        self.in_flight.clone()
    }

    /// How many calls are awaiting a reply *on this runtime*, whichever agent
    /// issued them. Test observability for the tracking that makes
    /// [`Self::cancel_in_flight`] possible.
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Tell the runtime to abandon every call still awaiting a reply.
    ///
    /// Dropping a tool future abandons it *locally* only: without this the sandbox
    /// keeps running the command to completion, holding resources, with its output
    /// discarded. Best-effort and non-blocking on failure — the caller is already
    /// tearing the turn down.
    /// Only *this agent's* calls, never the runtime's. The map is shared by
    /// every client on one sandbox, so cancelling everything here would abort a
    /// sibling subagent's tool call mid-flight.
    pub async fn cancel_in_flight(&self) {
        for id in &self.in_flight.of_agent(&self.agent_id) {
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
    ) -> Result<ScanResponse, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self
            .inner
            .scan_workspace(
                &call_id,
                &self.agent_id,
                workspace,
                instruction_candidates,
                skills_glob,
            )
            .await;
        self.untrack(&call_id);
        outcome.map_err(RuntimeCallError::Transport)
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
        self.track(&call_id);
        let outcome = self.inner.run_hooks(&call_id, &self.agent_id, &event).await;
        self.untrack(&call_id);
        let records = outcome.map_err(RuntimeCallError::Transport)?;
        if let Some(sink) = &self.hook_sink
            && !records.is_empty()
        {
            sink.record(records.clone()).await;
        }
        Ok(records)
    }

    /// List the tools the loaded plugins' MCP servers offer, and name the
    /// servers that could not be reached.
    pub async fn mcp_discover(
        &self,
    ) -> Result<horsie_models::runtime::McpDiscoverResponse, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.track(&call_id);
        let outcome = self.inner.mcp_discover(&call_id, &self.agent_id).await;
        self.untrack(&call_id);
        outcome.map_err(RuntimeCallError::Transport)
    }

    /// Call one namespaced plugin MCP tool.
    ///
    /// Tracked like an ordinary tool call, so a cancel reaches it: an MCP server
    /// that goes silent must not be un-stoppable.
    pub async fn mcp_invoke(
        &self,
        call_id: &str,
        tool: &str,
        arguments: String,
    ) -> Result<String, RuntimeCallError> {
        self.track(call_id);
        let outcome = self
            .inner
            .mcp_invoke(call_id, &self.agent_id, tool, arguments)
            .await;
        self.untrack(call_id);
        match outcome.map_err(RuntimeCallError::Transport)? {
            ToolResult::Ok(output) => Ok(output.stdout),
            ToolResult::Err(ToolError { reason }) => Err(RuntimeCallError::ToolFailed(reason)),
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
    fn probe_call() -> ToolCall {
        ToolCall::Bash(horsie_models::runtime::BashInput {
            command: "true".into(),
            timeout_secs: None,
        })
    }

    /// Every server-initiated command has to be visible to the reconciler while
    /// it runs — not just tool calls.
    ///
    /// The runtime registers `ScanWorkspace`, `RunHooks` and `McpDiscover` in its
    /// own in-flight map and reports all of them in a `Pong`. The reconciler
    /// cancels every id the runtime reports that has no issuer here, as the
    /// orphan a node restart leaves behind. So a command the client never
    /// tracked is a *live* call the reconciler cancels out from under its own
    /// caller — an MCP discovery that starts a fleet of servers, or a
    /// `SessionStart` hook, killed mid-flight the moment a sibling agent happens
    /// to have a tool call outstanding on the same runtime.
    ///
    /// One assertion per command rather than one for the set, so adding a
    /// command without deciding whether it is tracked shows up as a missing
    /// line here.
    #[tokio::test]
    async fn every_server_initiated_command_is_tracked_while_it_runs() {
        assert_tracked("provision_workspace", |c| async move {
            let _ = c.provision_workspace(Vec::new()).await;
        })
        .await;
        assert_tracked("scan_workspace", |c| async move {
            let _ = c
                .scan_workspace(None, Vec::new(), "*.md".to_string(), false)
                .await;
        })
        .await;
        assert_tracked("run_hooks", |c| async move {
            let _ = c
                .run_hooks(ServerHookEvent::SessionStart(
                    horsie_models::runtime::SessionStartInput {
                        source: horsie_models::runtime::SessionStartSource::Startup,
                    },
                ))
                .await;
        })
        .await;
        assert_tracked("mcp_discover", |c| async move {
            let _ = c.mcp_discover().await;
        })
        .await;
    }

    /// Run `call` against a gated transport and assert it is tracked for exactly
    /// as long as it is in flight.
    async fn assert_tracked<F, Fut>(label: &str, call: F)
    where
        F: FnOnce(RuntimeClient) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let gate = crate::testkit::BlockHandle::new();
        let in_flight = Arc::new(InFlight::new());
        let c = RuntimeClient::new(
            crate::testkit::MockTransport::gated_prep(&gate),
            "test-agent",
            in_flight.clone(),
        );

        let running = tokio::spawn(call(c));
        // Let it reach the transport and register.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            in_flight.of_agent("test-agent").len(),
            1,
            "{label} must be visible to the reconciler while it runs, or it is \
             cancelled as an orphan"
        );

        gate.release();
        let _ = running.await;
        assert_eq!(in_flight.len(), 0, "{label} must untrack once it answers");
    }

    #[tokio::test]
    async fn cancel_in_flight_reaches_every_pending_call() {
        // The gate holds two invokes open so both are genuinely in flight when the
        // cancel arrives — the shape a Stop mid-batch produces.
        let gate = crate::testkit::BlockHandle::new();
        let c = RuntimeClient::detached(
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
        let c = RuntimeClient::detached(crate::testkit::MockTransport::ok(""), "test-agent");
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
        let c = RuntimeClient::detached(MockTransport::disconnect_after(0), "test-agent");
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
        let client = RuntimeClient::detached(MockTransport::ok("").observed_by(&probe), "agent-1");
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
        let client = RuntimeClient::detached(MockTransport::ok("").observed_by(&probe), "agent-1");
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
        let client = RuntimeClient::detached(
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
        let parent = RuntimeClient::detached(MockTransport::ok("").observed_by(&probe), "main");
        let sub = parent.clone().with_agent_id("sub-1");
        parent.invoke("tc1", probe_call()).await.unwrap();
        sub.invoke("tc2", probe_call()).await.unwrap();
        assert_eq!(
            probe.agent_ids(),
            vec!["main".to_string(), "sub-1".to_string()]
        );
    }

    /// Why the in-flight map keys `call_id → agent_id` rather than being a set.
    ///
    /// The map is shared by every client on one runtime, so a `cancel_in_flight`
    /// that took all of it would abort a sibling subagent's tool call mid-flight —
    /// and the caller doing this is a subagent being stopped, which must leave its
    /// parent and its siblings running.
    #[tokio::test]
    async fn cancelling_one_agent_leaves_its_siblings_calls_alone() {
        let probe = crate::testkit::TransportProbe::new();
        let in_flight = Arc::new(InFlight::new());
        let parent = RuntimeClient::new(
            MockTransport::ok("").observed_by(&probe),
            "parent",
            in_flight.clone(),
        );
        let child = parent.clone().with_agent_id("child");
        in_flight.track("p1", "parent");
        in_flight.track("c1", "child");

        child.cancel_in_flight().await;

        assert_eq!(
            probe.cancels(),
            vec!["c1".to_string()],
            "only the cancelling agent's own call may be abandoned"
        );
        assert_eq!(
            in_flight.of_agent("parent"),
            vec!["p1".to_string()],
            "the parent's call is still outstanding"
        );
    }

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::detached(MockTransport::ok("hello"), "test-agent");
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
        let client = RuntimeClient::detached(MockTransport::err("oops"), "test-agent");
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
        let client =
            RuntimeClient::detached(MockTransport::ok("").with_scan(vec![scan]), "test-agent");
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
