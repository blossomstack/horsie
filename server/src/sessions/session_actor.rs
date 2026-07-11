//! One interactive session: lifecycle state machine, the only emitter of
//! runtime vendor signals, and host of the reused (interactive-mode)
//! [`AgentActor`].
//!
//! Recovery is lazy: `on_recovery_complete` only reconciles a mid-turn crash
//! (`Running` → `Interrupted`); no vendor call and no agent spawn happens until
//! the next user action ("a user message means make it run").

use crate::sessions::events::SessionEventSink;
use crate::sessions::spec::{ServerDeps, SessionSpec, SessionStatus};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::{SessionFrame, UserMessageError};
use crate::vendor::{RuntimeSpec, RuntimeVendor, VendorRuntime};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::workflow::WorkflowAgentDef;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentOutcome, AgentOutcomeSink, AgentParams, AgentRuntimeContext,
    DefaultToolboxFactory, SharedContext, ToolboxFactory, compose_system_prompt, scan_workspace,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// Capacity of a session's live frame broadcast. Slow subscribers see `lagged`
/// drops and catch up from the journal.
const FRAME_BROADCAST_CAPACITY: usize = 256;

/// Commands accepted by a [`SessionActor`].
pub enum SessionCommand {
    /// Provision the runtime after creation (sent once by the supervisor).
    Provision,
    /// A user message: answer a pending ask, or start a turn — attaching or
    /// re-provisioning whatever is missing first.
    UserMessage {
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
    },
    /// Stop: cancel any turn and stop the runtime, preserving it.
    Stop { reply: oneshot::Sender<()> },
    /// Delete: cancel, stop, and let the vendor decide the runtime's fate.
    Delete { reply: oneshot::Sender<()> },
    /// Hand back a live frame subscriber for the SSE stream.
    Subscribe {
        reply: oneshot::Sender<broadcast::Receiver<SessionFrame>>,
    },
    /// Tear down OS resources for a clean server shutdown; no status persisted,
    /// so a `Running` session reconciles to `Interrupted` next start.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: the hosted agent reported its terminal outcome.
    AgentOutcome(AgentOutcome),
    /// Internal: post-recovery reconciliation (`Running` → `Interrupted`).
    ReconcileInterrupted,
}

/// Events recording a session's lifecycle. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDomainEvent {
    Provisioned,
    ProvisionFailed {
        error: String,
    },
    TurnStarted,
    TurnCompleted,
    TurnFailed {
        error: String,
    },
    Asked {
        tool_call_id: Option<String>,
        question: String,
    },
    Interrupted,
    AttachFailed {
        error: String,
    },
    Stopped,
    Deleted,
}

/// Persisted session state — purely a function of the event log. `status` is
/// `None` until the first event (a freshly-created session still provisioning).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub status: Option<SessionStatus>,
    /// The ask tool-call id awaiting the user's answer (status AwaitingInput).
    pub pending_ask: Option<String>,
    pub pending_question: Option<String>,
    pub last_error: Option<String>,
}

/// Whether a wake provisions fresh or revives preserved state.
enum WakeMode {
    Create,
    Attach,
}

pub struct SessionActor {
    id: Uuid,
    spec: SessionSpec,
    deps: ServerDeps,
    parent: ActorRef<SessionSupervisorCommand>,
    frames: broadcast::Sender<SessionFrame>,
    runtime: Option<VendorRuntime>,
    agent: Option<ActorRef<AgentCommand>>,
}

impl SessionActor {
    pub fn new(
        id: Uuid,
        spec: SessionSpec,
        deps: ServerDeps,
        parent: ActorRef<SessionSupervisorCommand>,
    ) -> Self {
        let (frames, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        Self {
            id,
            spec,
            deps,
            parent,
            frames,
            runtime: None,
            agent: None,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor registry and the live stream.
    async fn report(&self, status: SessionStatus) {
        let _ = self.frames.send(SessionFrame::Status {
            status: status.clone(),
        });
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status,
            })
            .await;
    }

    fn vendor(&self) -> Result<Arc<dyn RuntimeVendor>, String> {
        self.deps
            .vendors
            .get(&self.spec.vendor)
            .cloned()
            .ok_or_else(|| format!("unknown runtime vendor '{}'", self.spec.vendor))
    }

    /// Write the capability file (the durable source of truth for re-attach) and
    /// assemble the vendor-facing runtime spec.
    fn write_runtime_spec(&self) -> Result<RuntimeSpec, String> {
        let dir = self
            .deps
            .state_dir
            .join("sessions")
            .join(self.id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let caps_path = dir.join("capabilities.json");
        std::fs::write(
            &caps_path,
            serde_json::to_vec_pretty(&self.spec.capabilities).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(RuntimeSpec {
            workspaces: self.spec.workspaces.clone(),
            capabilities_file: caps_path,
            plugins_dir: self.spec.plugins_dir.clone(),
            hook_path: self.spec.hook_path.clone(),
        })
    }

    /// Ensure a live runtime, emitting the explicit vendor signal for `mode`.
    async fn ensure_runtime(&mut self, mode: WakeMode) -> Result<(), String> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let vendor = self.vendor()?;
        let rt_spec = self.write_runtime_spec()?;
        let id = self.id.to_string();
        let runtime = match mode {
            WakeMode::Create => vendor.create(&id, &rt_spec).await,
            WakeMode::Attach => vendor.attach(&id, &rt_spec).await,
        }
        .map_err(|e| e.to_string())?;
        self.runtime = Some(runtime);
        Ok(())
    }

    /// Ensure a live agent child (recovering its conversation from the journal
    /// on respawn). Mirrors the workflow's `spawn_agent`, in interactive mode.
    async fn ensure_agent(&mut self, ctx: &ActorContext<Self>) -> Result<(), String> {
        if self.agent.is_some() {
            return Ok(());
        }
        let Some(runtime) = &self.runtime else {
            return Err("no live runtime".to_string());
        };
        let runtime_client = runtime.runtime_client.clone();
        let settings = &self.spec.agent;
        let provider = self
            .deps
            .provider_registry
            .get(&settings.model)
            .cloned()
            .ok_or_else(|| format!("no provider registered for model '{}'", settings.model))?;
        let def = WorkflowAgentDef {
            use_plugins: settings.use_plugins,
            name: "agent".to_string(),
            system_prompt: settings.system_prompt.clone(),
            model: settings.model.clone(),
            output_schema: None,
            allow_ask_user: settings.allow_ask_user,
            allow_timers: None,
            transitions: None,
            max_iterations: settings.max_iterations,
            max_retries: Some(settings.max_retries),
            allowed_tools: settings.allowed_tools.clone(),
        };
        let use_plugins = settings.use_plugins.unwrap_or(true);
        let (ws, shared_skills) = scan_workspace(&runtime_client, None, use_plugins).await;
        let shared = if use_plugins {
            let bootstrap = match runtime_client.run_session_start().await {
                Ok(context) if !context.trim().is_empty() => Some(context),
                Ok(_) | Err(_) => None,
            };
            Some(SharedContext {
                skills: Arc::new(shared_skills),
                bootstrap,
            })
        } else {
            None
        };
        let toolbox =
            DefaultToolboxFactory.for_agent(&def, runtime_client.clone(), ws.names(), use_plugins);
        let mut params = AgentParams::from_def(&def);
        params.interactive = true;
        params.system_prompt =
            compose_system_prompt(def.system_prompt.as_deref(), &ws, shared.as_ref());
        let agent_ctx = AgentRuntimeContext {
            provider,
            toolbox,
            event_sink: Arc::new(SessionEventSink {
                frames: self.frames.clone(),
            }),
            parent: Arc::new(SessionParent(ctx.self_ref())),
            session_id: self.id,
        };
        self.agent = Some(ctx.spawn(AgentActor::new(agent_ctx, params)));
        Ok(())
    }

    async fn wake(&mut self, ctx: &ActorContext<Self>, mode: WakeMode) -> Result<(), String> {
        self.ensure_runtime(mode).await?;
        self.ensure_agent(ctx).await
    }

    /// Start a fresh turn with `text` and reply to the caller.
    async fn start_turn(
        &mut self,
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let Some(agent) = &self.agent {
            let _ = agent.tell(AgentCommand::Run { input: text }).await;
        }
        let _ = reply.send(Ok(()));
        self.report(SessionStatus::Running).await;
        CommandEffect::persist(vec![SessionDomainEvent::TurnStarted])
    }

    async fn on_user_message(
        &mut self,
        state: &SessionState,
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        match state.status.clone() {
            Some(SessionStatus::Running) => {
                let _ = reply.send(Err(UserMessageError::TurnInFlight));
                CommandEffect::none()
            }
            // Answer a pending ask. Idempotent-resume: stay AwaitingInput until
            // the agent's own outcome persists the next state (a crash between
            // the inject and the agent's durable input would otherwise wedge).
            Some(SessionStatus::AwaitingInput) if state.pending_ask.is_some() => {
                let tool_call_id = state.pending_ask.clone().unwrap_or_default();
                match self.wake(ctx, WakeMode::Attach).await {
                    Ok(()) => {
                        if let Some(agent) = &self.agent {
                            let _ = agent
                                .tell(AgentCommand::InjectToolResult {
                                    tool_call_id,
                                    content: text,
                                })
                                .await;
                        }
                        let _ = reply.send(Ok(()));
                        CommandEffect::none()
                    }
                    Err(e) => {
                        let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                        self.report(SessionStatus::RecoveryFailed { reason: e.clone() })
                            .await;
                        CommandEffect::persist(vec![SessionDomainEvent::AttachFailed { error: e }])
                    }
                }
            }
            // Never provisioned (or provisioning went stale across a restart, or
            // failed): make it run by provisioning fresh.
            None | Some(SessionStatus::Provisioning) | Some(SessionStatus::Failed { .. }) => {
                match self.wake(ctx, WakeMode::Create).await {
                    Ok(()) => self.start_turn(text, reply).await,
                    Err(e) => {
                        let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                        self.report(SessionStatus::Failed { reason: e.clone() })
                            .await;
                        CommandEffect::persist(vec![SessionDomainEvent::ProvisionFailed {
                            error: e,
                        }])
                    }
                }
            }
            // Idle/Stopped/Interrupted/RecoveryFailed (and AwaitingInput with no
            // recorded ask id): revive preserved state and run the turn.
            Some(SessionStatus::Idle)
            | Some(SessionStatus::Stopped)
            | Some(SessionStatus::Interrupted)
            | Some(SessionStatus::RecoveryFailed { .. })
            | Some(SessionStatus::AwaitingInput) => match self.wake(ctx, WakeMode::Attach).await {
                Ok(()) => self.start_turn(text, reply).await,
                Err(e) => {
                    let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                    self.report(SessionStatus::RecoveryFailed { reason: e.clone() })
                        .await;
                    CommandEffect::persist(vec![SessionDomainEvent::AttachFailed { error: e }])
                }
            },
        }
    }

    async fn on_agent_outcome(
        &mut self,
        outcome: AgentOutcome,
    ) -> CommandEffect<SessionDomainEvent> {
        match outcome {
            AgentOutcome::Concluded { .. } => {
                // The agent actor stopped itself; a later turn respawns it and
                // recovers the conversation from the journal.
                self.agent = None;
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnCompleted])
            }
            AgentOutcome::Asked {
                tool_call_id,
                question,
                ..
            } => {
                self.report(SessionStatus::AwaitingInput).await;
                CommandEffect::persist(vec![SessionDomainEvent::Asked {
                    tool_call_id,
                    question,
                }])
            }
            AgentOutcome::Failed { error, .. } => {
                self.agent = None;
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                // A failed turn never bricks the session: back to Idle with the
                // error recorded; the user just sends another message.
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnFailed { error }])
            }
            AgentOutcome::Parked { .. } => {
                // Sessions run with timers off, so a park should be impossible.
                let error = "agent parked; timers are not supported in sessions".to_string();
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnFailed { error }])
            }
        }
    }

    /// Cancel any in-flight turn and stop the runtime (preserving it).
    async fn halt(&mut self) {
        if let Some(agent) = &self.agent {
            let _ = agent.tell(AgentCommand::Cancel).await;
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.handle.stop().await;
        }
        self.agent = None;
    }
}

/// Adapts the session's mailbox to the [`AgentOutcomeSink`] its agent reports to.
struct SessionParent(ActorRef<SessionCommand>);

#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self.0.tell(SessionCommand::AgentOutcome(outcome)).await;
    }
}

#[async_trait]
impl EventSourcedActor for SessionActor {
    type Command = SessionCommand;
    type Event = SessionDomainEvent;
    type State = SessionState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.id)
    }

    fn initial_state() -> SessionState {
        SessionState::default()
    }

    fn apply_event(mut state: SessionState, event: SessionDomainEvent) -> SessionState {
        match event {
            SessionDomainEvent::Provisioned => state.status = Some(SessionStatus::Idle),
            SessionDomainEvent::ProvisionFailed { error } => {
                state.status = Some(SessionStatus::Failed {
                    reason: error.clone(),
                });
                state.last_error = Some(error);
            }
            SessionDomainEvent::TurnStarted => {
                state.status = Some(SessionStatus::Running);
                state.pending_ask = None;
                state.pending_question = None;
            }
            SessionDomainEvent::TurnCompleted => {
                state.status = Some(SessionStatus::Idle);
                state.pending_ask = None;
                state.pending_question = None;
            }
            SessionDomainEvent::TurnFailed { error } => {
                state.status = Some(SessionStatus::Idle);
                state.last_error = Some(error);
            }
            SessionDomainEvent::Asked {
                tool_call_id,
                question,
            } => {
                state.status = Some(SessionStatus::AwaitingInput);
                state.pending_ask = tool_call_id;
                state.pending_question = Some(question);
            }
            SessionDomainEvent::Interrupted => state.status = Some(SessionStatus::Interrupted),
            SessionDomainEvent::AttachFailed { error } => {
                state.status = Some(SessionStatus::RecoveryFailed {
                    reason: error.clone(),
                });
                state.last_error = Some(error);
            }
            SessionDomainEvent::Stopped => state.status = Some(SessionStatus::Stopped),
            SessionDomainEvent::Deleted => {}
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SessionCommand::Provision => match self.ensure_runtime(WakeMode::Create).await {
                Ok(()) => {
                    self.report(SessionStatus::Idle).await;
                    CommandEffect::persist(vec![SessionDomainEvent::Provisioned])
                }
                Err(e) => {
                    self.report(SessionStatus::Failed { reason: e.clone() })
                        .await;
                    CommandEffect::persist(vec![SessionDomainEvent::ProvisionFailed { error: e }])
                }
            },
            SessionCommand::UserMessage { text, reply } => {
                self.on_user_message(state, text, reply, ctx).await
            }
            SessionCommand::Stop { reply } => {
                if state.status == Some(SessionStatus::Stopped) {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                self.halt().await;
                let _ = reply.send(());
                self.report(SessionStatus::Stopped).await;
                CommandEffect::persist(vec![SessionDomainEvent::Stopped])
            }
            SessionCommand::Delete { reply } => {
                self.halt().await;
                if let Ok(vendor) = self.vendor() {
                    vendor.delete(&self.id.to_string()).await;
                }
                let _ = reply.send(());
                // No status report: the supervisor removes the registry row.
                CommandEffect::persist_and_stop(vec![SessionDomainEvent::Deleted])
            }
            SessionCommand::Subscribe { reply } => {
                let _ = reply.send(self.frames.subscribe());
                CommandEffect::none()
            }
            SessionCommand::Shutdown { reply } => {
                self.halt().await;
                let _ = reply.send(());
                // No status persisted: a Running session reconciles to
                // Interrupted on the next start.
                CommandEffect::stop()
            }
            SessionCommand::AgentOutcome(outcome) => self.on_agent_outcome(outcome).await,
            SessionCommand::ReconcileInterrupted => {
                if state.status == Some(SessionStatus::Running) {
                    self.report(SessionStatus::Interrupted).await;
                    CommandEffect::persist(vec![SessionDomainEvent::Interrupted])
                } else {
                    CommandEffect::none()
                }
            }
        }
    }

    /// Lazy recovery: no vendor calls, no agent spawn. Only reconcile a
    /// mid-turn crash so the session list is immediately honest.
    async fn on_recovery_complete(&mut self, state: &SessionState, ctx: &mut ActorContext<Self>) {
        if state.status == Some(SessionStatus::Running) {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileInterrupted)
                .await;
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
    use crate::sessions::spec::AgentSettings;
    use crate::vendor::mock::MockVendor;
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    use std::collections::HashMap;

    /// A trivial supervisor stand-in that forwards status reports to a channel.
    struct NullSupervisor {
        statuses: tokio::sync::mpsc::UnboundedSender<SessionStatus>,
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Empty {}

    #[async_trait]
    impl EventSourcedActor for NullSupervisor {
        type Command = SessionSupervisorCommand;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("null-supervisor", "test")
        }
        fn initial_state() -> Empty {
            Empty {}
        }
        fn apply_event(state: Empty, _e: ()) -> Empty {
            state
        }
        async fn handle_command(
            &mut self,
            _state: &Empty,
            cmd: SessionSupervisorCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            if let SessionSupervisorCommand::SessionStatusChanged { status, .. } = cmd {
                let _ = self.statuses.send(status);
            }
            CommandEffect::none()
        }
    }

    fn spec_fixture(vendor: &str) -> SessionSpec {
        SessionSpec {
            name: None,
            agent: AgentSettings {
                model: "mock".into(),
                system_prompt: None,
                allowed_tools: None,
                allow_ask_user: false,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
            },
            workspaces: vec![],
            capabilities: CapabilitySpec {
                network: NetworkPolicy::Block(BlockNetwork {}),
                grants: vec![],
                unsafe_seatbelt_rules: None,
            },
            vendor: vendor.into(),
            plugins_dir: None,
            hook_path: vec![],
        }
    }

    struct Harness {
        actor: ActorRef<SessionCommand>,
        vendor: Arc<MockVendor>,
        statuses: tokio::sync::mpsc::UnboundedReceiver<SessionStatus>,
        id: Uuid,
        _tmp: tempfile::TempDir,
    }

    fn harness_on(journal: Arc<dyn Journal>, vendor: MockVendor) -> Harness {
        harness_with_id(journal, vendor, Uuid::new_v4())
    }

    fn harness_with_id(journal: Arc<dyn Journal>, vendor: MockVendor, id: Uuid) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = Arc::new(vendor);
        let mut vendors: HashMap<String, Arc<dyn crate::vendor::RuntimeVendor>> = HashMap::new();
        vendors.insert("mock".into(), vendor.clone());
        let deps = ServerDeps {
            provider_registry: HashMap::new(),
            vendors,
            state_dir: tmp.path().to_path_buf(),
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let parent = spawn_root(NullSupervisor { statuses: tx }, journal.clone());
        let actor = spawn_root(
            SessionActor::new(id, spec_fixture("mock"), deps, parent),
            journal,
        );
        Harness {
            actor,
            vendor,
            statuses: rx,
            id,
            _tmp: tmp,
        }
    }

    #[test]
    fn fold_covers_all_transitions() {
        use SessionDomainEvent as E;
        let s = SessionActor::apply_event(SessionState::default(), E::Provisioned);
        assert_eq!(s.status, Some(SessionStatus::Idle));
        let s = SessionActor::apply_event(s, E::TurnStarted);
        assert_eq!(s.status, Some(SessionStatus::Running));
        let s = SessionActor::apply_event(
            s,
            E::Asked {
                tool_call_id: Some("tc".into()),
                question: "q?".into(),
            },
        );
        assert_eq!(s.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(s.pending_ask.as_deref(), Some("tc"));
        assert_eq!(s.pending_question.as_deref(), Some("q?"));
        let s = SessionActor::apply_event(s, E::TurnCompleted);
        assert_eq!(s.status, Some(SessionStatus::Idle));
        assert_eq!(s.pending_ask, None);
        let s = SessionActor::apply_event(s, E::Interrupted);
        assert_eq!(s.status, Some(SessionStatus::Interrupted));
        let s = SessionActor::apply_event(
            s,
            E::AttachFailed {
                error: "gone".into(),
            },
        );
        assert!(matches!(
            s.status,
            Some(SessionStatus::RecoveryFailed { .. })
        ));
        assert_eq!(s.last_error.as_deref(), Some("gone"));
        let s = SessionActor::apply_event(s, E::Stopped);
        assert_eq!(s.status, Some(SessionStatus::Stopped));
        let s = SessionActor::apply_event(
            s,
            E::TurnFailed {
                error: "boom".into(),
            },
        );
        assert_eq!(s.status, Some(SessionStatus::Idle));
        assert_eq!(s.last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn provision_emits_create_signal_and_stop_preserves() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), MockVendor::new());
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        h.actor
            .ask(|reply| SessionCommand::Stop { reply })
            .await
            .unwrap();
        let sid = h.id.to_string();
        assert_eq!(
            h.vendor.signals(),
            vec![format!("create:{sid}"), format!("stop:{sid}")]
        );
        // Status reports arrived in order: Idle (provisioned) then Stopped.
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);
    }

    #[tokio::test]
    async fn delete_signals_vendor_discretion() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), MockVendor::new());
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        h.actor
            .ask(|reply| SessionCommand::Delete { reply })
            .await
            .unwrap();
        let sid = h.id.to_string();
        assert_eq!(
            h.vendor.signals(),
            vec![
                format!("create:{sid}"),
                format!("stop:{sid}"),
                format!("delete:{sid}")
            ]
        );
        // Only the provisioned Idle was reported; delete removes rather than reports.
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
    }

    #[tokio::test]
    async fn provision_failure_lands_failed_status() {
        let mut h = harness_on(
            Arc::new(InMemoryJournal::new()),
            MockVendor::new().fail_create(),
        );
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        match h.statuses.recv().await.unwrap() {
            SessionStatus::Failed { reason } => assert!(reason.contains("mock create failure")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_reconciles_running_to_interrupted_without_vendor_calls() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        // Simulate a mid-turn crash: the previous incarnation journaled
        // Provisioned + TurnStarted and then died.
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![
            serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
            serde_json::to_vec(&SessionDomainEvent::TurnStarted).unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();

        let mut h = harness_with_id(journal, MockVendor::new(), id);
        // Recovery reconciles to Interrupted...
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Interrupted);
        // ...without any vendor signal (lazy recovery).
        assert!(h.vendor.signals().is_empty());
    }

    #[tokio::test]
    async fn message_on_recovered_session_attaches_and_fails_visibly_on_attach_error() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap()];
        journal.persist(&pid, &events).await.unwrap();

        let mut h = harness_with_id(journal, MockVendor::new().fail_attach_times(1), id);
        // Idle after recovery; a message triggers attach, which fails once.
        let res = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(matches!(res, Err(UserMessageError::RecoveryFailed(_))));
        match h.statuses.recv().await.unwrap() {
            SessionStatus::RecoveryFailed { reason } => {
                assert!(reason.contains("mock attach failure"));
            }
            other => panic!("expected RecoveryFailed, got {other:?}"),
        }
        let sid = h.id.to_string();
        assert_eq!(h.vendor.signals(), vec![format!("attach:{sid}")]);
    }

    #[tokio::test]
    async fn message_while_running_conflicts() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![
            serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
            serde_json::to_vec(&SessionDomainEvent::TurnStarted).unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();
        let h = harness_with_id(journal, MockVendor::new(), id);
        // Race the reconcile: send the message before ReconcileInterrupted may
        // have processed — both orders are valid; accept either error.
        let res = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        match res {
            Err(UserMessageError::TurnInFlight) => {}
            // Reconcile won the race → Interrupted → attach path (mock succeeds).
            Ok(()) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
