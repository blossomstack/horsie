//! One interactive session: lifecycle state machine, the only emitter of
//! runtime vendor signals, and host of the reused (interactive-mode)
//! [`AgentActor`].
//!
//! Recovery is lazy: `on_recovery_complete` only reconciles a mid-turn crash
//! (`Running` → `Interrupted`); no vendor call and no agent spawn happens until
//! the next user action ("a user message means make it run").

use crate::sessions::ask_tool::{ASK_USER_TOOL, AskUserToolbox};
use crate::sessions::events::SessionEventSink;
use crate::sessions::spec::{ServerDeps, SessionSpec, SessionStatus};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::{SessionFrame, UserMessageError};
use crate::vendor::{RuntimeSpec, RuntimeVendor, VendorRuntime};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::Toolbox;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentOutcome, AgentOutcomeSink, AgentParams, AgentRunDef,
    AgentRuntimeContext, DefaultToolboxFactory, SharedContext, ToolboxFactory,
    compose_system_prompt, scan_workspace,
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
        let vendors = self
            .deps
            .vendors
            .read()
            .map_err(|_| "vendor registry lock poisoned".to_string())?;
        vendors
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
            workspaces: self
                .spec
                .workspaces
                .iter()
                .map(|w| crate::vendor::WorkspaceSpec {
                    name: w.name.clone(),
                    source: match &w.path {
                        Some(p) => crate::vendor::WorkspaceSource::HostDir(p.clone()),
                        None => crate::vendor::WorkspaceSource::Managed,
                    },
                })
                .collect(),
            provision: self
                .spec
                .provision
                .iter()
                .map(|s| horsie_models::executor::ProvisionStep {
                    name: s.name.clone(),
                    uses: s.uses.clone(),
                    with: s
                        .with
                        .iter()
                        .map(|(k, v)| horsie_models::executor::StepParam {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                })
                .collect(),
            env: vec![],
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
        let mut rt_spec = self.write_runtime_spec()?;
        // Fresh, scoped token at every create AND attach — never persisted. It
        // authorizes the `git_checkout` provision steps for github.com repos.
        if let Some(minter) = &self.deps.github_tokens {
            let urls: Vec<String> = rt_spec
                .provision
                .iter()
                .filter(|s| s.uses == "git_checkout")
                .filter_map(|s| {
                    s.with
                        .iter()
                        .find(|p| p.key == "url")
                        .map(|p| p.value.clone())
                })
                .collect();
            if !urls.is_empty()
                && let Some(token) = minter.mint_for(&urls).await?
            {
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_GITHUB_TOKEN.to_string(),
                    value: token,
                });
            }
        }
        let id = self.id.to_string();
        // Resolve the session's selected bundles → fetch refs + a scoped token,
        // injected as env the runtime reads at startup. Same for create and
        // attach (re-resolves current versions). Only vendors that advertise an
        // artifact base URL participate (mock does not).
        if let (Some(prov), Some(base)) = (self.deps.plugins.as_ref(), vendor.artifact_base_url()) {
            let mut names = self.spec.plugins.clone();
            if names.is_empty() {
                names = prov.default_names().await;
            }
            if !names.is_empty() {
                let refs = prov.resolve(&names, &base).await?;
                let hashes: Vec<String> = refs.iter().map(|r| r.hash.clone()).collect();
                let token = prov.mint_token(&id, &hashes);
                let manifest = serde_json::to_string(&refs).map_err(|e| e.to_string())?;
                let mut env = vec![
                    horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGIN_MANIFEST.to_string(),
                        value: manifest,
                    },
                    horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGINS_TOKEN.to_string(),
                        value: token,
                    },
                ];
                if let Some(dir) = vendor.plugins_dir_for(&id) {
                    env.push(horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGINS_DIR.to_string(),
                        value: dir,
                    });
                }
                if let Some(cache) = vendor.plugins_cache_dir() {
                    env.push(horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGINS_CACHE.to_string(),
                        value: cache,
                    });
                }
                rt_spec.env.extend(env);
            }
        }
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
        // Resolve the provider from the shared registry under a short-lived read
        // guard (dropped before the awaits below), so live config edits take
        // effect on the next turn.
        let provider = {
            let reg = self
                .deps
                .provider_registry
                .read()
                .map_err(|_| "provider registry lock poisoned".to_string())?;
            reg.get(&settings.model).cloned()
        }
        .ok_or_else(|| format!("no provider registered for model '{}'", settings.model))?;
        // A session is not a workflow graph node -- it has no `name`/`model`/
        // `transitions`, so it builds an `AgentRunDef` directly rather than a
        // `WorkflowAgentDef`. `allow_ask_user` stays `false`: sessions get their
        // own dedicated, always-available `ask_user` tool (below) instead of the
        // workflow crate's `conclude`-based ask mechanism.
        let def = AgentRunDef {
            system_prompt: settings.system_prompt.clone(),
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
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
        // Connect the session's enabled MCP servers and expose their tools next
        // to the runtime tools (subject to the same allowlist). Done per spawn,
        // like the workspace scan, so tools reflect the live servers each turn.
        let mcp: Vec<Arc<dyn Toolbox>> = if settings.mcp_servers.is_empty() {
            Vec::new()
        } else if let Some(mcp_svc) = self.deps.mcp.as_ref() {
            mcp_svc
                .toolboxes_for(&settings.mcp_servers)
                .await
                .map_err(|e| format!("build MCP toolboxes: {e}"))?
        } else {
            tracing::warn!(
                session = %self.id,
                "session names MCP servers but no MCP service is configured; ignoring"
            );
            Vec::new()
        };
        let toolbox: Arc<dyn Toolbox> =
            Arc::new(AskUserToolbox::new(DefaultToolboxFactory.for_agent(
                &def,
                runtime_client.clone(),
                ws.names(),
                use_plugins,
                mcp,
            )));
        let mut params = AgentParams::from_def(&def);
        params.interactive = true;
        params.optional_handoff_tool = Some(ASK_USER_TOOL.to_string());
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
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
            },
            workspaces: vec![],
            provision: vec![],
            capabilities: CapabilitySpec {
                network: NetworkPolicy::Block(BlockNetwork {}),
                grants: vec![],
                unsafe_seatbelt_rules: None,
            },
            vendor: vendor.into(),
            plugins_dir: None,
            hook_path: vec![],
            plugins: vec![],
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
        harness_custom(journal, vendor, id, spec_fixture("mock"), None)
    }

    fn harness_custom(
        journal: Arc<dyn Journal>,
        vendor: MockVendor,
        id: Uuid,
        spec: SessionSpec,
        github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    ) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = Arc::new(vendor);
        let mut vendors: HashMap<String, Arc<dyn crate::vendor::RuntimeVendor>> = HashMap::new();
        vendors.insert("mock".into(), vendor.clone());
        let deps = ServerDeps {
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors: Arc::new(std::sync::RwLock::new(vendors)),
            state_dir: tmp.path().to_path_buf(),
            github_tokens,
            mcp: None,
            plugins: None,
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let parent = spawn_root(NullSupervisor { statuses: tx }, journal.clone());
        let actor = spawn_root(SessionActor::new(id, spec, deps, parent), journal);
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

    struct FixedMinter(Option<String>);
    #[async_trait]
    impl crate::github::GithubTokenMinter for FixedMinter {
        async fn mint_for(&self, _repo_urls: &[String]) -> Result<Option<String>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn provision_mints_github_token_into_env() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let mut spec = spec_fixture("mock");
        spec.provision = vec![crate::sessions::spec::ProvisionStepSpec {
            name: "checkout api".into(),
            uses: "git_checkout".into(),
            with: vec![
                ("url".into(), "https://github.com/o/api".into()),
                ("dir".into(), "api".into()),
            ],
        }];
        let mut h = harness_custom(
            journal,
            MockVendor::new(),
            Uuid::new_v4(),
            spec,
            Some(Arc::new(FixedMinter(Some("ghs_x".into())))),
        );
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
        let spec = h.vendor.last_create_spec().expect("vendor saw a spec");
        assert!(
            spec.env
                .iter()
                .any(|e| e.name == horsie_models::ENV_GITHUB_TOKEN && e.value == "ghs_x"),
            "GITHUB_TOKEN injected: {:?}",
            spec.env
        );
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
