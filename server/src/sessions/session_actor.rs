//! One interactive session: the conversational state machine and the owner of
//! its agents.
//!
//! Three things are deliberately *not* here. The session does not know how a
//! runtime is provisioned, resumed or released — that is
//! [`RuntimeManager`](crate::runtime_manager::RuntimeManager), and no vendor
//! call ever runs on this mailbox. It does not decide when it is loaded or
//! unloaded — that is the supervisor. And it never resumes a turn by itself:
//! an interrupted assistant turn is over, while accepted user input is a
//! promise kept at the next turn boundary.

use crate::runtime_manager::{RuntimeClientProvider, RuntimeError};
use crate::sessions::ask_tool::{ASK_USER_TOOL, AskUserToolbox};
use crate::sessions::events::{AgentEventSink, BroadcastObserver};
use crate::sessions::mode::SessionModeState;
use crate::sessions::orchestrator::{
    AgentAction, InteractiveOrchestrator, Orchestrator, SessionCommandKind,
};
use crate::sessions::spawn_tool::SubAgentToolbox;
use crate::sessions::spec::{AgentSettings, PendingAsk, ServerDeps, SessionSpec, SessionStatus};
use crate::sessions::subagents::{INTERRUPTED_ERROR, MAX_SUBAGENT_DEPTH, SubAgentParent};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::{SessionTitleToolbox, normalize_session_title};
use crate::sessions::{AgentFrame, SessionFrame, UserMessageError};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_models::agent::ToolResultInput;
use horsie_models::hooks::HookRecord;
use horsie_models::hooks::{HookAction, StopOutcome, SubagentStopOutcome};
use horsie_models::now_ms;
use horsie_models::runtime::{
    ServerHookEvent, SessionStartInput, StopInput, SubagentStartInput, SubagentStopInput,
    UserPromptSubmitInput,
};
use horsie_runtime_client::RuntimeClient;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentHistoryPage, AgentOutcome, AgentOutcomeSink, AgentParams,
    AgentRunDef, AgentRuntimeContext, AgentUsageSnapshot, ContextError, ContextProvider, Contexts,
    DefaultToolboxFactory, HistoryQuery, SharedContext, StartTurn, ToolboxFactory, UsageTotal,
    compose_system_prompt, scan_workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// Capacity of a session's live frame broadcast. Slow subscribers see `lagged`
/// drops and catch up from the journal.
pub(crate) const FRAME_BROADCAST_CAPACITY: usize = 256;

/// The agent id a session's primary agent reports usage under.
const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn emit_progress(frames: &broadcast::Sender<SessionFrame>, stage: &str, detail: Option<String>) {
    let _ = frames.send(SessionFrame::Progression {
        stage: stage.to_string(),
        detail,
        at_ms: now_ms(),
    });
}

/// The baseline system prompt given to every session agent.
const SESSION_AGENT_PROMPT: &str = include_str!("system_prompt.md");

/// Commands accepted by a [`SessionActor`].
pub enum SessionCommand {
    /// A user message. Always accepted: it is queued durably and answered by
    /// the next turn, so there is no rejection path and no `409`.
    UserMessage {
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    },
    /// Cancel the turn in flight. Queued messages are *not* discarded — stop
    /// means "not this turn", not "throw away what I asked for".
    Stop { reply: oneshot::Sender<()> },
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: oneshot::Sender<()> },
    /// Read a window of conversation history from one of the session's
    /// agents: `agent_id` absent or `"main"` for the primary agent, otherwise
    /// a subagent id. `None` answers "no such agent".
    History {
        agent_id: Option<String>,
        query: HistoryQuery,
        reply: oneshot::Sender<Option<AgentHistoryPage>>,
    },
    /// Read this session's aggregated usage.
    UsageStats {
        reply: oneshot::Sender<SessionUsageStats>,
    },
    /// Answer every pending ask at once, resuming the turn.
    Answer {
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<Result<(), AnswerError>>,
    },
    /// Read this session's recovered state: status, pending ask, inbox.
    Snapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },
    /// Subscribe to one agent's live frames. `agent_id` is `None`/`"main"` for
    /// the primary agent, else a subagent id; the outer `None` means no such
    /// agent. A cold subagent is spawned on demand, exactly as `History` does.
    SubscribeAgent {
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<broadcast::Receiver<AgentFrame>>>,
    },
    /// Read one agent's current values (task list, usage) for its document.
    AgentState {
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<horsie_workflow::AgentStateView>>,
    },
    /// The supervisor wants to unload this session. Answers `false` if a run
    /// started in the meantime, in which case nothing has changed and the idle
    /// clock simply restarts.
    PrepareOffload { reply: oneshot::Sender<bool> },
    /// Internal: an agent reported its terminal outcome.
    AgentOutcome(AgentOutcome),
    /// Plugin hooks ran against one agent's tool call. Pure routing: the session
    /// persists nothing here, it forwards to the agent whose transcript the
    /// records belong in. Carries no reply because nothing waits on it.
    HooksRan {
        key: AgentKey,
        records: Vec<HookRecord>,
    },
    /// A `Stop` hook blocked, so the turn continues with `reason` as its input.
    ///
    /// Routed through the session for the same reason `HooksRan` is: the sink is
    /// built before its `AgentActor` is spawned, so it holds a key rather than
    /// an `ActorRef`.
    ContinueAfterStop { key: AgentKey, reason: String },
    /// A hook set `continue: false`, so this agent stops where it is.
    ///
    /// The session is the only thing that can act on it: the runtime that ran
    /// the hook has no way to end a turn, and the agent is mid-call. What
    /// stopping *means* is per key — a turn boundary for the main agent, a
    /// failed node for a subagent, a failed step for a step.
    HaltAgent { key: AgentKey, reason: String },
    /// Internal: post-recovery reconciliation of a turn the process died in.
    ReconcileInterrupted,
    /// Set the session title from the built-in title tool.
    SetSessionTitle {
        title: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// The `spawn_agent` tool: start a subagent under `caller`.
    SpawnSubAgent {
        caller: SubAgentParent,
        label: String,
        task: String,
        /// A plugin-declared agent type, already checked against the catalogue
        /// by the tool that advertised it. The session journals the name and
        /// never resolves it: what an agent type *is* belongs to the plugin
        /// library as of the moment the subagent runs, not the moment it was
        /// asked for.
        agent_type: Option<String>,
        reply: oneshot::Sender<Result<Uuid, String>>,
    },
    /// The `subagent_status` tool: one node, or the caller's whole subtree.
    SubAgentStatus {
        caller: SubAgentParent,
        id: Option<Uuid>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Read the whole subagent tree (backs `GET /api/sessions/:id/subagents`).
    /// Read this session's workflow run, if it is one.
    RunState {
        reply: oneshot::Sender<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Let the orchestrator start whatever it wants started. Sent to a run at
    /// load so a pending one begins, and after a retry.
    AdvanceRun,
    /// Re-run one execution from the run log.
    RetryStep {
        index: u32,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SubAgentTree {
        reply: oneshot::Sender<Vec<(Uuid, crate::sessions::subagents::SubAgentRecord)>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under (tree nodes still `Running`). Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    ReconcileSubAgents,
    /// Internal: the spawn's `SubAgentSpawned` write came back — only now
    /// does the child actor exist (persist-then-spawn). A failed write spawns
    /// nothing and the tool gets the error.
    FinishSpawnSubAgent {
        id: Uuid,
        label: String,
        task: String,
        agent_type: Option<String>,
        reply: oneshot::Sender<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
}

/// Events recording a session's lifecycle. Persisted.
///
/// Every variant carries `at_ms`, the unix-epoch millisecond it was recorded,
/// so a journal pulled off a server reconstructs a timeline and not just an
/// order. Stamped where the event is built, immediately before it is persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDomainEvent {
    /// A user message was accepted. Durable *before* anything is done with it,
    /// so an accepted message survives a crash and is still owed an answer.
    MessageQueued {
        id: String,
        text: String,
        at_ms: u64,
    },
    /// A turn started, consuming these queued messages — and, if the session
    /// was parked on an ask, answering it. One event so a crash anywhere in
    /// the window replays to the same place.
    TurnBegan {
        at_ms: u64,
        consumed: Vec<String>,
        /// The single ask this turn answered. Kept for journals written before
        /// a turn could answer several; new turns write `answered`.
        answering: Option<String>,
        /// Every ask this turn answered. Empty when the turn abandoned them or
        /// there were none.
        #[serde(default)]
        answered: Vec<String>,
    },
    /// The agent asked the user something and is parked on it.
    AskRecorded {
        at_ms: u64,
        tool_call_id: Option<String>,
        question: String,
    },
    TurnEnded {
        at_ms: u64,
    },
    TurnFailed {
        at_ms: u64,
        error: String,
    },
    /// The user cancelled the turn. Distinct from `TurnEnded` only in intent;
    /// both are turn boundaries, and both let the inbox drain.
    TurnStopped {
        at_ms: u64,
    },
    /// Recovery found a turn that the process died in. Recorded rather than
    /// inferred, so the transition is in the log like every other one.
    TurnInterrupted {
        at_ms: u64,
    },
    /// Terminal: this session can never run again.
    SessionFailed {
        at_ms: u64,
        reason: String,
    },
    /// One agent's cumulative usage after a completed run. Durable here so the
    /// session-level total never requires waking an idle agent.
    UsageRecorded {
        at_ms: u64,
        agent_id: String,
        usage_total: UsageTotal,
    },
    /// A subagent was spawned by `parent` (the main agent or another
    /// subagent). Persisted before the child actor exists — a crash between
    /// the two replays as a node that recovery reconciles to failed.
    SubAgentSpawned {
        at_ms: u64,
        id: Uuid,
        parent: SubAgentParent,
        label: String,
        task: String,
        depth: u32,
        /// The plugin-declared agent type this subagent runs as, if any.
        /// Defaulted so journals written before typed agents existed replay as
        /// the general-purpose subagent they were.
        #[serde(default)]
        agent_type: Option<String>,
    },
    /// A terminal node started another run, woken to consume child results.
    SubAgentRunning {
        at_ms: u64,
        id: Uuid,
    },
    SubAgentCompleted {
        at_ms: u64,
        id: Uuid,
        output: String,
    },
    SubAgentFailed {
        at_ms: u64,
        id: Uuid,
        error: String,
    },
    /// The node's latest terminal result was sent to its parent. Persisted in
    /// the same effect as the send, so a reload neither re- nor never-sends.
    SubAgentNotified {
        at_ms: u64,
        id: Uuid,
    },
    /// One execution of one workflow step began. Appended, never replacing: a
    /// loop back onto a step and a retry of one are both new entries, which is
    /// what keeps the log replayable and the graph projection lossless.
    StepStarted {
        at_ms: u64,
        index: u32,
        step: String,
        agent: Uuid,
        attempt: u32,
        /// The entry this came out of; `None` for the start step.
        from: Option<u32>,
        /// The transition condition that matched, if any.
        via: Option<String>,
        input: String,
    },
    StepConcluded {
        at_ms: u64,
        index: u32,
        output: Value,
    },
    StepFailed {
        at_ms: u64,
        index: u32,
        error: String,
    },
    /// A step was cancelled — by an interrupt, or by a retry taking its place.
    /// Suspends the run: a person decides between retrying and abandoning,
    /// because the step's effect on the shared workspace is unknown.
    StepCancelled {
        at_ms: u64,
        index: u32,
    },
    RunFinished {
        at_ms: u64,
        output: Value,
    },
    RunFailed {
        at_ms: u64,
        error: String,
    },
}

/// One answer to one pending ask.
#[derive(Debug, Clone)]
pub struct AskAnswer {
    pub tool_call_id: String,
    pub text: String,
}

/// Why a set of answers was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The session is not parked on anything answerable.
    NothingPending,
    /// The answers did not cover the pending asks exactly.
    Incomplete {
        missing: Vec<String>,
        unexpected: Vec<String>,
    },
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingPending => write!(f, "this session is not waiting on an answer"),
            Self::Incomplete {
                missing,
                unexpected,
            } => write!(
                f,
                "every pending question must be answered together (missing: [{}]; not pending: [{}])",
                missing.join(", "),
                unexpected.join(", ")
            ),
        }
    }
}

/// One accepted-but-undelivered user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: String,
    pub text: String,
    pub at_ms: u64,
}

/// Persisted session state — purely a function of the event log.
///
/// `#[serde(default)]` on the container: this is snapshotted, so it is a
/// durability contract, and a container default fills anything a future version
/// adds. Add optional fields; never rename or repurpose one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub status: SessionStatus,
    /// Every ask awaiting an answer (status `AwaitingInput`), oldest first. A
    /// turn may ask several questions at once, and the run cannot resume until
    /// all of them have a result.
    #[serde(default)]
    pub pending_asks: Vec<PendingAsk>,
    /// Accepted user messages not yet delivered to a turn. The client shows
    /// these as unread; they go in with whatever turn starts next.
    #[serde(default)]
    pub inbox: Vec<InboxMessage>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub agent_usage: HashMap<String, UsageTotal>,
    /// What drives this session's agents, and the subagent tree beneath it.
    /// Was a bare `subagents` field; [`SessionModeState`] still reads that
    /// shape, so snapshots written before the move load unchanged.
    #[serde(default)]
    pub mode: SessionModeState,
}

/// One agent's own usage/context-size snapshot, labeled with the model it ran.
#[derive(Debug, Clone)]
pub struct AgentUsageEntry {
    pub model: String,
    pub snapshot: AgentUsageSnapshot,
}

/// What a reader needs to know about a session, answered by the actor that owns
/// it. Every field is recovered from the journal, so an unloaded session gives
/// the same answers as a loaded one — it just has to be loaded to give them.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    /// Carries the pending asks when the session is parked on questions.
    pub status: SessionStatus,
    pub inbox: Vec<InboxMessage>,
}

/// A session's aggregated usage.
#[derive(Debug, Clone)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    pub main_agent: AgentUsageEntry,
}

/// Longest auto-derived session title, in characters.
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;

/// A short title derived from a user's first message.
fn derive_title(text: &str) -> Option<String> {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    if first_line.chars().count() <= TITLE_MAX_CHARS {
        return Some(first_line.to_string());
    }
    let truncated: String = first_line.chars().take(TITLE_MAX_CHARS).collect();
    Some(format!("{}…", truncated.trim_end()))
}

/// Which agent of a session a broadcast belongs to. `Main` is not a `Uuid`
/// variant because the main agent's journal is keyed by the *session* id — the
/// two namespaces are deliberately distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKey {
    Main,
    Sub(Uuid),
    /// One execution of a workflow step. Its own key, not `Sub`: a step is not
    /// spawned by an agent, it is chosen by the definition, and it roots a
    /// subagent tree of its own.
    Step(Uuid),
}

/// The agent actors a session hosts.
///
/// An enum rather than an `Option` plus a map: a session's topology is decided
/// at creation and never changes, and a workflow run has no main agent at all.
enum SessionAgents {
    Interactive {
        main: ActorRef<AgentCommand>,
        subs: HashMap<Uuid, ActorRef<AgentCommand>>,
    },
    /// A workflow run: step agents and their subagents, all keyed by id. There
    /// is no main agent — the definition, not a person, decides who runs.
    Workflow {
        live: HashMap<Uuid, ActorRef<AgentCommand>>,
    },
}

impl SessionAgents {
    fn interactive(main: ActorRef<AgentCommand>) -> Self {
        Self::Interactive {
            main,
            subs: HashMap::new(),
        }
    }

    fn workflow() -> Self {
        Self::Workflow {
            live: HashMap::new(),
        }
    }

    /// The session's primary agent, for the kinds that have one.
    fn main(&self) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { main, .. } => Some(main),
            Self::Workflow { .. } => None,
        }
    }

    fn sub(&self, id: Uuid) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { subs, .. } => subs.get(&id),
            Self::Workflow { live } => live.get(&id),
        }
    }

    /// The agent registered under `key`, if it is still resident.
    fn get(&self, key: AgentKey) -> Option<&ActorRef<AgentCommand>> {
        match key {
            AgentKey::Main => self.main(),
            AgentKey::Sub(id) | AgentKey::Step(id) => self.sub(id),
        }
    }

    fn insert_sub(&mut self, id: Uuid, agent: ActorRef<AgentCommand>) {
        match self {
            Self::Interactive { subs, .. } => {
                subs.insert(id, agent);
            }
            Self::Workflow { live } => {
                live.insert(id, agent);
            }
        }
    }

    /// Every agent, emptying the set. Used when the session unloads.
    fn drain_all(&mut self) -> Vec<ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { main, subs } => {
                let mut out: Vec<_> = subs.drain().map(|(_, a)| a).collect();
                out.push(main.clone());
                out
            }
            Self::Workflow { live } => live.drain().map(|(_, a)| a).collect(),
        }
    }
}

pub struct SessionActor {
    id: Uuid,
    spec: SessionSpec,
    deps: ServerDeps,
    parent: ActorRef<SessionSupervisorCommand>,
    frames: broadcast::Sender<SessionFrame>,
    /// The agent actors this session hosts, resident for as long as this actor
    /// is loaded. `None` means exactly one thing — recovery has not finished —
    /// which is why the topology inside is a value rather than a second
    /// `Option`: a session's shape is decided at creation and never changes.
    agents: Option<SessionAgents>,
    /// The step being started, for the instant between the orchestrator naming
    /// it and the `StepStarted` event landing in the log. Only
    /// `perform_run_action` sets it, and it clears it in the same call.
    pending_step: Option<(u32, String)>,
    /// Decides what this session runs next. Chosen at construction from the
    /// spec; the actor only performs what it returns.
    orchestrator: Arc<dyn Orchestrator>,
    /// The main agent's context provider, kept so [`Self::cancel_run`] can
    /// reach the runtime client the run already acquired instead of asking
    /// the manager for a fresh one.
    context_provider: Option<Arc<SessionContextProvider>>,
    /// One live broadcast per agent. Created with the agent and kept for this
    /// actor's loaded lifetime, so a subscriber survives a turn ending. A
    /// session nobody watches still publishes — into a channel with no
    /// receivers, which costs nothing.
    agent_frames: HashMap<AgentKey, broadcast::Sender<AgentFrame>>,
}

impl SessionActor {
    /// `frames` is owned by the supervisor and outlives this actor: a session
    /// that unloads under a watching client must not disconnect it.
    pub fn new(
        id: Uuid,
        spec: SessionSpec,
        deps: ServerDeps,
        parent: ActorRef<SessionSupervisorCommand>,
        frames: broadcast::Sender<SessionFrame>,
    ) -> Self {
        // The spec decides which of the two this session is, once and for all.
        let orchestrator: Arc<dyn Orchestrator> = match &spec.workflow {
            Some(run) => Arc::new(crate::sessions::workflow::WorkflowOrchestrator::new(
                id,
                run.clone(),
            )),
            None => Arc::new(InteractiveOrchestrator),
        };
        Self {
            id,
            spec,
            deps,
            parent,
            frames,
            agents: None,
            pending_step: None,
            orchestrator,
            context_provider: None,
            agent_frames: HashMap::new(),
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor's cache and the live stream.
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

    /// Publish the queue as it stands once `events` fold onto `state`.
    ///
    /// One frame per command, carrying the whole queue — never an intermediate.
    /// A message that is accepted and immediately drained therefore publishes
    /// an empty inbox once, rather than "queued" followed by "gone", which a
    /// client would render as a flicker. A no-op when nothing touched the queue,
    /// so callers may call it unconditionally.
    fn publish_inbox(&self, state: &SessionState, events: &[SessionDomainEvent]) {
        if !events.iter().any(|e| {
            matches!(
                e,
                SessionDomainEvent::MessageQueued { .. } | SessionDomainEvent::TurnBegan { .. }
            )
        }) {
            return;
        }
        let next = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        let _ = self
            .frames
            .send(SessionFrame::InboxChanged { queued: next.inbox });
    }

    /// Persist a session title through the supervisor, then publish it.
    async fn rename_session(&mut self, title: String) -> Result<String, String> {
        let id = self.id.to_string();
        let persisted = self
            .parent
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: title.clone(),
                reply,
            })
            .await
            .map_err(|e| format!("session supervisor unavailable: {e}"))?;
        persisted.map_err(|e| format!("persist session title: {e}"))?;

        self.spec.name = Some(title.clone());
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::PublishSessionTitle {
                id,
                name: title.clone(),
            })
            .await;
        Ok(title)
    }

    /// Spawn the resident agent. Cheap and runtime-free: the provider, toolbox
    /// and system prompt are resolved per run, on the run's own task.
    fn spawn_main_agent(&mut self, ctx: &ActorContext<Self>) {
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            step_output_schema: None,
            session_id: self.id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            frames: self.frames.clone(),
            last_client: Mutex::new(None),
        });
        self.context_provider = Some(context_provider.clone());
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        // Only when the tool exists: an unattended session is not offered
        // `ask_user`, and naming a handoff tool the toolbox does not carry
        // would leave the loop watching for a call that can never come.
        if !self.spec.is_unattended() {
            params.optional_handoff_tool = Some(ASK_USER_TOOL.to_string());
        }
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let frames = self.agent_frames(AgentKey::Main);
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            event_sink: Arc::new(AgentEventSink {
                frames: frames.clone(),
            }),
            parent: Arc::new(StopHookParent {
                inner: Arc::new(SessionParent {
                    target: ctx.self_ref(),
                }),
                session: ctx.self_ref(),
                key: AgentKey::Main,
                provider: context_provider.clone(),
                continuations: Arc::new(AtomicUsize::new(0)),
            }),
            session_id: self.id,
        };
        self.agents = Some(SessionAgents::interactive(ctx.spawn(
            AgentActor::with_observer(agent_ctx, params, Arc::new(BroadcastObserver { frames })),
        )));
    }

    /// Spawn the agent for one execution of a workflow step.
    ///
    /// Differs from a subagent in three ways, all of them the point: it runs
    /// with its *own* preset's settings rather than the session's, it carries
    /// the step's output schema so `conclude` is typed, and it is keyed as a
    /// step so it roots its own subagent tree.
    fn spawn_step_agent(
        &mut self,
        ctx: &ActorContext<Self>,
        index: u32,
        agent_id: Uuid,
    ) -> Option<ActorRef<AgentCommand>> {
        let run_spec = self.spec.workflow.clone()?;
        let step_name = {
            // The name comes from the log when the entry exists (recovery), and
            // from the definition's order otherwise.
            let by_index = run_spec.steps.get(index as usize).map(|s| s.name.clone());
            self.pending_step
                .as_ref()
                .filter(|(i, _)| *i == index)
                .map(|(_, name)| name.clone())
                .or(by_index)?
        };
        let step = run_spec.step(&step_name)?.clone();
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: step.settings.clone(),
            step_output_schema: step.output_schema.clone(),
            session_id: self.id,
            kind: SessionAgentKind::Step(agent_id),
            agent_type: None,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            frames: self.frames.clone(),
            last_client: Mutex::new(None),
        });
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            // The schema is what makes `conclude` typed, and typed output is
            // what a transition condition reads.
            output_schema: step.output_schema.clone(),
            // Asking rides on `conclude`, so only a step that already has one
            // can ask. A step that declares no output ends its turn with plain
            // text, and that text is its output — forcing a terminal tool on it
            // would fail the run the moment the model simply answered.
            allow_ask_user: step.output_schema.is_some() && !self.spec.is_unattended(),
            allow_timers: None,
            max_iterations: step.settings.max_iterations,
            max_retries: Some(step.settings.max_retries),
            allowed_tools: step.settings.allowed_tools.clone(),
        });
        params.interactive = true;
        // Deliberately no handoff tool. A step's terminal tool is `conclude`,
        // synthesized from its output schema; naming `ask_user` here would stop
        // the loop treating `conclude` as terminal, so it would try to *execute*
        // it, get "the conclude tool is terminal and is not executed" back, and
        // keep going. A step asks through `conclude(kind=ask)` instead.
        params.thinking_effort = step
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let frames = self.agent_frames(AgentKey::Step(agent_id));
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            event_sink: Arc::new(AgentEventSink {
                frames: frames.clone(),
            }),
            parent: Arc::new(StopHookParent {
                inner: Arc::new(SessionParent {
                    target: ctx.self_ref(),
                }),
                session: ctx.self_ref(),
                key: AgentKey::Step(agent_id),
                provider: context_provider.clone(),
                continuations: Arc::new(AtomicUsize::new(0)),
            }),
            session_id: agent_id,
        };
        let actor = ctx.spawn(AgentActor::with_observer(
            agent_ctx,
            params,
            Arc::new(BroadcastObserver { frames }),
        ));
        if let Some(agents) = self.agents.as_mut() {
            agents.insert_sub(agent_id, actor.clone());
        }
        Some(actor)
    }

    /// Carry out a decision that belongs to a workflow run rather than to a
    /// turn: start a step, or end the run.
    async fn perform_run_action(
        &mut self,
        action: AgentAction,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        match action {
            AgentAction::StartStep {
                index,
                step,
                agent,
                attempt,
                from,
                via,
                input,
            } => {
                // The name is not in the log yet — this event is what puts it
                // there — so hand it to the spawner directly.
                self.pending_step = Some((index, step.clone()));
                let spawned = self.spawn_step_agent(ctx, index, agent);
                self.pending_step = None;
                let Some(actor) = spawned else {
                    return vec![SessionDomainEvent::RunFailed {
                        at_ms: now_ms(),
                        error: format!("step '{step}' is no longer in this workflow"),
                    }];
                };
                if actor
                    .tell(AgentCommand::Resume {
                        results: Vec::new(),
                        message: Some(input.clone()),
                        subagent_results: Vec::new(),
                    })
                    .await
                    .is_err()
                {
                    return vec![SessionDomainEvent::RunFailed {
                        at_ms: now_ms(),
                        error: format!("step '{step}' could not be started"),
                    }];
                }
                emit_progress(&self.frames, "step_started", Some(step.clone()));
                self.report(SessionStatus::Running).await;
                vec![SessionDomainEvent::StepStarted {
                    at_ms: now_ms(),
                    index,
                    step,
                    agent,
                    attempt,
                    from,
                    via,
                    input,
                }]
            }
            AgentAction::Finish { output } => {
                self.report(SessionStatus::Idle).await;
                vec![SessionDomainEvent::RunFinished {
                    at_ms: now_ms(),
                    output,
                }]
            }
            AgentAction::Fail { error } => {
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                vec![SessionDomainEvent::RunFailed {
                    at_ms: now_ms(),
                    error,
                }]
            }
            AgentAction::StartTurn { .. } => unreachable!("handled by `perform`"),
        }
    }

    /// The broadcast for one agent, created on first use. Held by the session
    /// rather than the agent so a subscriber outlives the agent's turn.
    fn agent_frames(&mut self, key: AgentKey) -> broadcast::Sender<AgentFrame> {
        self.agent_frames
            .entry(key)
            .or_insert_with(|| broadcast::channel(FRAME_BROADCAST_CAPACITY).0)
            .clone()
    }

    /// Resolve an agent selector to its resident actor: `None`/`"main"` for the
    /// primary agent, else a subagent id. A cold node — one in the persisted
    /// tree with no actor since this session loaded — is spawned on demand, so
    /// reading a finished subagent works exactly like reading a live one.
    fn resolve_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
        agent_id: Option<&str>,
    ) -> Option<(AgentKey, ActorRef<AgentCommand>)> {
        match agent_id {
            None | Some("main") => self
                .agents
                .as_ref()
                .and_then(SessionAgents::main)
                .cloned()
                .map(|a| (AgentKey::Main, a)),
            Some(raw) => {
                let id = Uuid::parse_str(raw).ok()?;
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
                    return Some((AgentKey::Sub(id), agent.clone()));
                }
                // The type comes off the record, not from the caller: a cold
                // node woken to answer a read must run as what it was spawned as.
                let agent_type = state.mode.subagents().get(&id)?.agent_type.clone();
                Some((
                    AgentKey::Sub(id),
                    self.spawn_sub_agent_actor(ctx, id, agent_type),
                ))
            }
        }
    }

    /// Spawn a resident subagent actor — journal replay only; the caller
    /// decides whether a run starts (spawn) or not (recovery).
    /// Spawn one subagent's actor. `agent_type` names a plugin-declared agent to
    /// run as, and travels no further than the provider: the *definition* is
    /// resolved from the library scan when the subagent runs, so an agent whose
    /// plugin was removed in between fails loudly rather than running with a
    /// prompt nobody can point at.
    fn spawn_sub_agent_actor(
        &mut self,
        ctx: &ActorContext<Self>,
        id: Uuid,
        agent_type: Option<String>,
    ) -> ActorRef<AgentCommand> {
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            step_output_schema: None,
            session_id: self.id,
            kind: SessionAgentKind::Sub(id),
            agent_type,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            frames: self.frames.clone(),
            last_client: Mutex::new(None),
        });
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        // No handoff tool: a subagent ends its turn with plain text, which
        // becomes the output its parent is notified with.
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let frames = self.agent_frames(AgentKey::Sub(id));
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            event_sink: Arc::new(AgentEventSink {
                frames: frames.clone(),
            }),
            parent: Arc::new(StopHookParent {
                inner: Arc::new(SessionParent {
                    target: ctx.self_ref(),
                }),
                session: ctx.self_ref(),
                key: AgentKey::Sub(id),
                provider: context_provider.clone(),
                continuations: Arc::new(AtomicUsize::new(0)),
            }),
            session_id: id,
        };
        let actor = ctx.spawn(AgentActor::with_observer(
            agent_ctx,
            params,
            Arc::new(BroadcastObserver { frames }),
        ));
        if let Some(agents) = self.agents.as_mut() {
            agents.insert_sub(id, actor.clone());
        }
        actor
    }

    fn agent(&self) -> Option<&ActorRef<AgentCommand>> {
        self.agents.as_ref().and_then(SessionAgents::main)
    }

    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self.agent() {
            Some(agent) => agent
                .ask(|reply| AgentCommand::GetUsage { reply })
                .await
                .unwrap_or_default(),
            None => AgentUsageSnapshot::default(),
        };
        let main_usage_total = state
            .agent_usage
            .get(MAIN_AGENT_ID)
            .copied()
            .unwrap_or_default();
        let session_total = state
            .agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u));
        SessionUsageStats {
            session_total,
            main_agent: AgentUsageEntry {
                model: self.spec.agent.model.clone(),
                snapshot: AgentUsageSnapshot {
                    usage_total: main_usage_total,
                    last_turn_usage: snapshot.last_turn_usage,
                    context_tokens: snapshot.context_tokens,
                },
            },
        }
    }

    /// Carry out one orchestrator decision: resume the agent it names, report
    /// the status it implies, and return the events that record it.
    ///
    /// The single place a turn ever begins. Reached only at turn boundaries (a
    /// message arriving while idle, a turn ending, a stop) — never on load,
    /// which is what keeps opening a session free of side effects.
    async fn perform(
        &mut self,
        action: AgentAction,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let AgentAction::StartTurn {
            who,
            input,
            consumed,
            answered,
            notified,
            mark_running,
        } = action
        else {
            return self.perform_run_action(action, ctx).await;
        };
        match who {
            // A subagent parent waking to consume its children's results. It
            // is skipped, not failed, when its actor cannot be reached: the
            // results stay owed and the next boundary retries.
            // A step is resumed only by `perform_run_action`; the turn path
            // never names one.
            AgentKey::Step(_) => Vec::new(),
            AgentKey::Sub(id) => {
                let agent = match self.agents.as_ref().and_then(|a| a.sub(id)) {
                    Some(agent) => agent.clone(),
                    // A cold node woken for the first time since load: spawn
                    // its resident actor on demand (see `on_recovery_complete`).
                    None => match state.mode.subagents().get(&id) {
                        Some(rec) => {
                            let agent_type = rec.agent_type.clone();
                            self.spawn_sub_agent_actor(ctx, id, agent_type)
                        }
                        None => return Vec::new(),
                    },
                };
                if agent
                    .tell(AgentCommand::Resume {
                        results: input.results,
                        message: input.message,
                        subagent_results: input.subagent_results,
                    })
                    .await
                    .is_err()
                {
                    return Vec::new();
                }
                let mut events = Vec::new();
                if let Some(parent) = mark_running {
                    events.push(SessionDomainEvent::SubAgentRunning {
                        at_ms: now_ms(),
                        id: parent,
                    });
                }
                events.extend(notified.into_iter().map(|id| {
                    SessionDomainEvent::SubAgentNotified {
                        at_ms: now_ms(),
                        id,
                    }
                }));
                events
            }
            AgentKey::Main => {
                if let Some(agent) = self.agent() {
                    let _ = agent
                        .tell(AgentCommand::Resume {
                            results: input.results,
                            message: input.message,
                            subagent_results: input.subagent_results,
                        })
                        .await;
                }
                self.report(SessionStatus::Running).await;
                // Tell-then-persist, like the user messages this turn also
                // carries: a crash between the agent's `Run` and this write
                // leaves the result owed, so the next turn re-delivers it.
                // Delivery is at-least-once in that window (the parent may see
                // a result twice), never lost — `spawn_agent`'s stricter
                // persist-then-spawn is the deliberate exception, because an
                // untracked agent is worse than a duplicate.
                let mut events = vec![SessionDomainEvent::TurnBegan {
                    at_ms: now_ms(),
                    consumed,
                    answering: None,
                    answered,
                }];
                events.extend(notified.into_iter().map(|id| {
                    SessionDomainEvent::SubAgentNotified {
                        at_ms: now_ms(),
                        id,
                    }
                }));
                events
            }
        }
    }

    /// Everything the orchestrator wants started at this turn boundary,
    /// performed in order, each seeing the state the previous one produced.
    ///
    /// Every turn boundary routes through here — without that, a result owed to
    /// a subagent parent strands the moment no further subagent outcome can
    /// arrive (every node terminal), since an outcome was previously the only
    /// flush trigger.
    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for action in self.orchestrator.next_actions(&next) {
            let produced = self.perform(action, &next, ctx).await;
            for e in &produced {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(produced);
        }
        events
    }

    /// Answer every pending ask at once and resume the turn. A set that does not
    /// cover the pending asks exactly is refused and nothing is journaled: a
    /// half-answered park would leave the run unable to resume and the wire
    /// holding a `tool_use` with no result.
    async fn on_answer(
        &mut self,
        state: &SessionState,
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<Result<(), AnswerError>>,
    ) -> CommandEffect<SessionDomainEvent> {
        let pending: HashSet<String> = state
            .pending_asks
            .iter()
            .filter_map(|a| a.tool_call_id.clone())
            .collect();
        if pending.is_empty() {
            let _ = reply.send(Err(AnswerError::NothingPending));
            return CommandEffect::none();
        }
        let answered: HashSet<String> = answers.iter().map(|a| a.tool_call_id.clone()).collect();
        if answered != pending {
            let mut missing: Vec<String> = pending.difference(&answered).cloned().collect();
            let mut unexpected: Vec<String> = answered.difference(&pending).cloned().collect();
            missing.sort();
            unexpected.sort();
            let _ = reply.send(Err(AnswerError::Incomplete {
                missing,
                unexpected,
            }));
            return CommandEffect::none();
        }

        let results: Vec<ToolResultInput> = answers
            .iter()
            .map(|a| ToolResultInput {
                tool_call_id: a.tool_call_id.clone(),
                output: a.text.clone(),
                is_error: false,
            })
            .collect();
        if let Some(agent) = self.agent() {
            let _ = agent
                .tell(AgentCommand::Resume {
                    results,
                    message: None,
                    subagent_results: Vec::new(),
                })
                .await;
        }
        self.report(SessionStatus::Running).await;
        let _ = reply.send(Ok(()));
        CommandEffect::persist(vec![SessionDomainEvent::TurnBegan {
            at_ms: now_ms(),
            consumed: Vec::new(),
            answering: None,
            answered: answers.into_iter().map(|a| a.tool_call_id).collect(),
        }])
    }

    async fn on_user_message(
        &mut self,
        state: &SessionState,
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let SessionStatus::Unrecoverable { reason } = &state.status {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // An unnamed session is titled from its first message, once.
        if self.spec.name.is_none()
            && let Some(title) = derive_title(&text)
            && let Err(error) = self.rename_session(title).await
        {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }

        let queued = SessionDomainEvent::MessageQueued {
            id: Uuid::new_v4().to_string(),
            text,
            at_ms: now_ms(),
        };
        let SessionDomainEvent::MessageQueued { id, .. } = &queued else {
            unreachable!("just constructed")
        };
        let message_id = id.clone();
        let _ = reply.send(Ok(message_id));

        // Fold the queue locally so the drain sees the message it is about to
        // persist — same fold the runtime will apply, just one step early.
        let next = Self::apply_event(state.clone(), queued.clone());
        let mut events = vec![queued];
        events.extend(self.flush_then_drain(&next, ctx).await);
        self.publish_inbox(state, &events);
        CommandEffect::persist(events)
    }

    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let outcome_session = match &outcome {
            AgentOutcome::Concluded { session_id, .. }
            | AgentOutcome::Asked { session_id, .. }
            | AgentOutcome::Parked { session_id }
            | AgentOutcome::Failed { session_id, .. }
            | AgentOutcome::UsageRecorded { session_id, .. } => *session_id,
        };
        // In a run, an outcome is a step's or one of a step's subagents'.
        if let Some(run) = state.mode.run() {
            if let Some(index) = run.index_of_agent(outcome_session) {
                return self.on_step_outcome(state, index, outcome, ctx).await;
            }
            return self
                .on_sub_agent_outcome(state, outcome_session, outcome, ctx)
                .await;
        }
        if outcome_session != self.id {
            return self
                .on_sub_agent_outcome(state, outcome_session, outcome, ctx)
                .await;
        }
        // Usage is always recorded: the tokens were spent whatever became of
        // the turn that spent them.
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                at_ms: now_ms(),
                agent_id: MAIN_AGENT_ID.to_string(),
                usage_total,
            }]);
        }
        let (mut events, drained) = match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { .. } => {
                self.report(SessionStatus::Idle).await;
                (
                    vec![SessionDomainEvent::TurnEnded { at_ms: now_ms() }],
                    true,
                )
            }
            AgentOutcome::Asked { asks, .. } => {
                self.report(SessionStatus::AwaitingInput {
                    asks: asks
                        .iter()
                        .map(|a| PendingAsk {
                            tool_call_id: a.tool_call_id.clone(),
                            question: a.question.clone(),
                        })
                        .collect(),
                })
                .await;
                (
                    asks.into_iter()
                        .map(|a| SessionDomainEvent::AskRecorded {
                            at_ms: now_ms(),
                            tool_call_id: a.tool_call_id,
                            question: a.question,
                        })
                        .collect::<Vec<_>>(),
                    // An ask is a turn boundary too: a message queued while the
                    // agent was working becomes the answer.
                    true,
                )
            }
            AgentOutcome::Failed {
                error, terminal, ..
            } => {
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                // A runtime that a live vendor cannot produce is the one
                // terminal failure: re-provisioning would silently rebuild a
                // workspace the user believes they still have. Everything else
                // — provider errors, tool errors, a vendor that is merely
                // offline — is a failed turn they can retry.
                if terminal {
                    self.report(SessionStatus::Unrecoverable {
                        reason: error.clone(),
                    })
                    .await;
                    (
                        vec![SessionDomainEvent::SessionFailed {
                            at_ms: now_ms(),
                            reason: error,
                        }],
                        false,
                    )
                } else {
                    self.report(SessionStatus::Failed {
                        reason: error.clone(),
                    })
                    .await;
                    // Deliberately no drain: a stuck cause (expired key, dead
                    // vendor) would otherwise turn three queued messages into
                    // three back-to-back failures. The next message drains them.
                    (
                        vec![SessionDomainEvent::TurnFailed {
                            at_ms: now_ms(),
                            error,
                        }],
                        false,
                    )
                }
            }
            AgentOutcome::Parked { .. } => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::TurnFailed {
                        at_ms: now_ms(),
                        error,
                    }],
                    false,
                )
            }
        };
        if drained {
            let mut next = state.clone();
            for e in &events {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(self.flush_then_drain(&next, ctx).await);
        }
        self.publish_inbox(state, &events);
        CommandEffect::persist(events)
    }

    /// Re-run one execution from the log.
    ///
    /// Appends rather than truncating: earlier attempts stay readable, and the
    /// graph renders them stacked on their node. A run still in flight has its
    /// current step cancelled first — the run's workspace is shared, so two
    /// steps must never be writing to it at once.
    ///
    /// The workspace itself is *not* rolled back. A retried step re-runs
    /// against whatever the previous attempt left on disk; that is the honest
    /// behaviour and the guide says so.
    async fn on_retry_step(
        &mut self,
        state: &SessionState,
        index: u32,
        reply: oneshot::Sender<Result<(), String>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some(run) = state.mode.run() else {
            let _ = reply.send(Err("this session is not a workflow run".into()));
            return CommandEffect::none();
        };
        let Some(target) = run.get(index).cloned() else {
            let _ = reply.send(Err(format!("no step execution at index {index}")));
            return CommandEffect::none();
        };
        let mut events = Vec::new();
        // Cancel whatever is in flight first, so the retry is the only writer.
        if let Some(current) = run.current() {
            if let Some(agent) = run
                .get(current)
                .and_then(|s| self.agents.as_ref().and_then(|a| a.sub(s.agent)))
                .cloned()
            {
                let (tx, rx) = oneshot::channel();
                let _ = agent.tell(AgentCommand::Cancel { ack: Some(tx) }).await;
                if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
                    tracing::warn!(
                        session = %self.id,
                        "cancelled step did not finish within {CANCEL_TIMEOUT:?}; proceeding"
                    );
                }
            }
            events.push(SessionDomainEvent::StepCancelled {
                at_ms: now_ms(),
                index: current,
            });
        }
        let mut next = state.clone();
        for e in &events {
            next = Self::apply_event(next, e.clone());
        }
        let new_index = next
            .mode
            .run()
            .map(|r| r.steps.len() as u32)
            .unwrap_or_default();
        let attempt = next
            .mode
            .run()
            .map(|r| r.attempts_of(&target.step) + 1)
            .unwrap_or(1);
        let action = AgentAction::StartStep {
            index: new_index,
            step: target.step.clone(),
            agent: crate::sessions::workflow::WorkflowRunSpec::step_agent_id(self.id, new_index),
            attempt,
            // The retry sits where the original sat, so the graph draws it on
            // the same edge rather than inventing a new one.
            from: target.from,
            via: target.via.clone(),
            input: target.input.clone(),
        };
        let _ = reply.send(Ok(()));
        events.extend(self.perform_run_action(action, ctx).await);
        CommandEffect::persist(events)
    }

    /// One step's outcome. Mechanical: map it onto the log entry that records
    /// it, then let the orchestrator read the folded state and decide what runs
    /// next. Every branching decision — which transition, whether the run is
    /// over — is in the driver, not here.
    async fn on_step_outcome(
        &mut self,
        state: &SessionState,
        index: u32,
        outcome: AgentOutcome,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        // Usage is always recorded: the tokens were spent whatever became of
        // the step that spent them.
        if let AgentOutcome::UsageRecorded {
            usage_total,
            session_id,
        } = outcome
        {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                at_ms: now_ms(),
                agent_id: session_id.to_string(),
                usage_total,
            }]);
        }
        let step_name = state
            .mode
            .run()
            .and_then(|r| r.get(index))
            .map(|s| s.step.clone())
            .unwrap_or_default();
        let (mut events, advance) = match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { output, .. } => {
                emit_progress(&self.frames, "step_concluded", Some(step_name));
                (
                    vec![SessionDomainEvent::StepConcluded {
                        at_ms: now_ms(),
                        index,
                        output,
                    }],
                    true,
                )
            }
            AgentOutcome::Asked { asks, .. } => {
                self.report(SessionStatus::AwaitingInput {
                    asks: asks
                        .iter()
                        .map(|a| PendingAsk {
                            tool_call_id: a.tool_call_id.clone(),
                            question: a.question.clone(),
                        })
                        .collect(),
                })
                .await;
                (
                    asks.into_iter()
                        .map(|a| SessionDomainEvent::AskRecorded {
                            at_ms: now_ms(),
                            tool_call_id: a.tool_call_id,
                            question: a.question,
                        })
                        .collect::<Vec<_>>(),
                    // The step is still running, parked on its question. The
                    // answer resumes it; nothing else starts meanwhile.
                    false,
                )
            }
            AgentOutcome::Failed { error, .. } => {
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                // A step that fails fails the run. Retrying it is a decision
                // for a person: the shared workspace holds whatever the failed
                // attempt left behind, so re-running blind would redo
                // half-finished work.
                (
                    vec![SessionDomainEvent::StepFailed {
                        at_ms: now_ms(),
                        index,
                        error,
                    }],
                    false,
                )
            }
            AgentOutcome::Parked { .. } => {
                let error = "step parked; timers are not supported in workflows".to_string();
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::StepFailed {
                        at_ms: now_ms(),
                        index,
                        error,
                    }],
                    false,
                )
            }
        };
        if advance {
            let mut next = state.clone();
            for e in &events {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(self.flush_then_drain(&next, ctx).await);
        }
        CommandEffect::persist(events)
    }

    /// A subagent's outcome: record it in the tree, then deliver every result
    /// owed to idle parents — wakes for subagent parents, a turn (via
    /// `drain`) when the main agent is owed and idle.
    async fn on_sub_agent_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        outcome: AgentOutcome,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                at_ms: now_ms(),
                agent_id: id.to_string(),
                usage_total,
            }]);
        }
        let Some(rec) = state.mode.subagents().get(&id).cloned() else {
            tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored");
            return CommandEffect::none();
        };
        let terminal = match outcome {
            AgentOutcome::Concluded { output, .. } => {
                let text = output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string());
                emit_progress(
                    &self.frames,
                    "subagent_completed",
                    Some(format!("\"{}\" ({id})", rec.label)),
                );
                SessionDomainEvent::SubAgentCompleted {
                    at_ms: now_ms(),
                    id,
                    output: text,
                }
            }
            AgentOutcome::Failed { error, .. } => {
                emit_progress(
                    &self.frames,
                    "subagent_failed",
                    Some(format!("\"{}\" ({id})", rec.label)),
                );
                SessionDomainEvent::SubAgentFailed {
                    at_ms: now_ms(),
                    id,
                    error,
                }
            }
            // Defensive: a subagent has no ask or timer tools, so neither
            // outcome should ever occur.
            AgentOutcome::Asked { .. } => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent asked the user; not supported".to_string(),
            },
            AgentOutcome::Parked { .. } => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent parked; timers are not supported in sessions".to_string(),
            },
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
        };
        let mut events = vec![terminal];
        let next = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        events.extend(self.flush_then_drain(&next, ctx).await);
        CommandEffect::persist(events)
    }

    /// Cancel the run in flight, if any, and wait for it to actually be over.
    ///
    /// Waiting matters: the caller is about to record a turn boundary, and a
    /// run that is still winding down can still append to the agent journal.
    async fn cancel_run(&mut self) {
        let Some(agent) = self.agent().cloned() else {
            return;
        };
        // Tell the sandbox to abandon what it is running first, so the wait
        // below is over an already-cancelled call rather than a live one. Uses
        // the client the run itself already acquired in `provide()` — asking
        // the manager for a fresh one would round-trip the vendor on this very
        // mailbox, and a vendor mid-tool-call cannot answer a lifecycle
        // request until the tool call it is relaying resolves.
        if let Some(client) = self
            .context_provider
            .as_ref()
            .and_then(|cp| cp.cached_client())
        {
            client.cancel_in_flight().await;
        }
        let (tx, rx) = oneshot::channel();
        let _ = agent.tell(AgentCommand::Cancel { ack: Some(tx) }).await;
        if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
            tracing::warn!(
                session = %self.id,
                "cancelled run did not finish within {CANCEL_TIMEOUT:?}; proceeding"
            );
        }
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    async fn stop_agents(&mut self) {
        let Some(mut agents) = self.agents.take() else {
            return;
        };
        for agent in agents.drain_all() {
            // Cancel first: a stopped mailbox makes the run task's next persist
            // fail, but an in-flight tool call would run to completion first.
            let _ = agent.tell(AgentCommand::Cancel { ack: None }).await;
            let _ = agent.tell(AgentCommand::Shutdown).await;
        }
    }
}

/// Adapts the session's mailbox to the [`AgentOutcomeSink`] its agents report
/// to. No generation tag: the agent is resident and fences its own stale runs
/// by `run_id`, so every outcome that arrives here is one the session asked for.
struct SessionParent {
    target: ActorRef<SessionCommand>,
}

/// Routes what plugin hooks did into the session's journal.
///
/// A `tell`, not an `ask`: nothing waits on a record, and a hook's audit trail
/// must never be able to slow the tool call it describes.
struct SessionHookSink {
    target: ActorRef<SessionCommand>,
    /// Which agent's transcript these records belong in. A subagent's hooks are
    /// its own; without this they would all pile into one log with no way to
    /// tell whose call they guarded.
    key: AgentKey,
}

#[async_trait]
impl horsie_runtime_client::HookSink for SessionHookSink {
    async fn record(&self, hooks: Vec<HookRecord>) {
        // A halt is read here rather than in the session's `HooksRan` handler
        // so that handler stays what it says it is: pure routing into an
        // agent's transcript.
        //
        // Tool records only. Every *server*-initiated event's records travel
        // this sink as well as being returned to the seam that fired them —
        // `RuntimeClient::run_hooks` does both — and each of those seams reads
        // the halt off its own return value: `start_blocked` at the pre-run
        // seam, `StopHookParent` at the stop seam. Acting on them here too
        // would halt the same agent twice, and on `Stop` it would fail a turn
        // the stop seam is deliberately ending cleanly.
        let halt = tool_halt_reason(&hooks);
        let _ = self
            .target
            .tell(SessionCommand::HooksRan {
                key: self.key,
                records: hooks,
            })
            .await;
        // After the records, so the transcript shows what halted the turn
        // above the turn's own failure.
        if let Some(reason) = halt {
            let _ = self
                .target
                .tell(SessionCommand::HaltAgent {
                    key: self.key,
                    reason,
                })
                .await;
        }
    }
}

#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self
            .target
            .tell(SessionCommand::AgentOutcome(outcome))
            .await;
    }
}

/// How many times a `Stop` hook may hold a turn open before horsie ends it
/// regardless.
///
/// Not advisory. horsie runs unattended sessions, and `stop_hook_active` only
/// stops a hook that reads it — this exists for the ones that do not.
const MAX_STOP_CONTINUATIONS: usize = 3;

/// Runs `Stop` hooks when a turn concludes, and honours what they say.
///
/// A decorator on the outcome sink rather than a branch in the session's
/// `AgentOutcome` handler, because `deliver` is called from the *agent's*
/// `RunFinished` handler. A slow hook therefore delays that agent's own mailbox
/// and never the session's command loop, which stays able to serve a cancel or
/// another agent while a 30-second `Stop` hook runs.
struct StopHookParent {
    inner: Arc<dyn AgentOutcomeSink>,
    session: ActorRef<SessionCommand>,
    key: AgentKey,
    /// The provider whose `provide()` cached this agent's client. `Stop` never
    /// acquires a runtime of its own: a turn that already concluded must not be
    /// able to fail on provisioning, and there is nothing to guard if no runtime
    /// ever ran.
    provider: Arc<SessionContextProvider>,
    /// Consecutive continuations. Reset whenever a turn concludes without a
    /// block, so a long interactive session that legitimately continues a few
    /// times never accumulates toward the cap.
    continuations: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentOutcomeSink for StopHookParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        // `Stop` fires when a turn *ends*. An ask or a park is a turn still in
        // progress, and a failure is not a stop the hook could act on.
        let AgentOutcome::Concluded { output, .. } = &outcome else {
            return self.inner.deliver(outcome).await;
        };
        // No plugins, or no runtime this turn: nothing declared a hook, so the
        // round-trip would be pure latency on every single turn.
        let Some(client) = self
            .provider
            .use_plugins()
            .then(|| self.provider.cached_client())
            .flatten()
        else {
            return self.inner.deliver(outcome).await;
        };

        let used = self.continuations.load(Ordering::Relaxed);
        let last_assistant_message = output.as_str().map(str::to_string);
        // The spec's own definition: true when horsie would normally stop but is
        // being held in the loop by a blocking hook. A cooperative hook returns
        // early on it.
        let stop_hook_active = used > 0;
        // A subagent's turn ending is a `SubagentStop`, not a `Stop`. This sink
        // decorates every agent a session hosts, and until it was gated on the
        // kind a session with four subagents fired `Stop` five times.
        let event = match self.provider.kind {
            SessionAgentKind::Sub(id) => ServerHookEvent::SubagentStop(SubagentStopInput {
                agent_id: id.to_string(),
                agent_type: self.provider.agent_type(),
                last_assistant_message,
                stop_hook_active,
            }),
            // A step keeps `Stop`: it fires `SessionStart` and roots its own
            // subagent tree, so answering `SubagentStop` would contradict its
            // own start.
            SessionAgentKind::Main | SessionAgentKind::Step(_) => {
                ServerHookEvent::Stop(StopInput {
                    last_assistant_message,
                    stop_hook_active,
                })
            }
        };
        let records = client.run_hooks(event).await.unwrap_or_default();

        // A halt outranks a block, which is the spec's own precedence: a hook
        // that says both is asking to stop, and the turn is already stopping.
        // So this ends the turn the way an unblocked one ends — the records are
        // already on their way to the transcript, `run_hooks` having put them on
        // the sink, and there is nothing to add to them the way `CapReached`
        // adds to a block.
        if let Some(reason) = halt_reason(&records) {
            self.continuations.store(0, Ordering::Relaxed);
            tracing::info!(reason, "a stop hook set continue: false; the turn ends");
            return self.inner.deliver(outcome).await;
        }

        match stop_verdict(&records) {
            // Blocked *from stopping*, with budget left: the turn does not
            // conclude. The parent never hears about it, so the session never
            // marks the turn done and never drains its queue early.
            Some(reason) if used < MAX_STOP_CONTINUATIONS => {
                self.continuations.fetch_add(1, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::ContinueAfterStop {
                        key: self.key,
                        reason,
                    })
                    .await;
            }
            // Blocked, but out of budget. The turn ends, and a second record
            // says why — otherwise this reads as a turn that stopped on its own.
            Some(_) => {
                self.continuations.store(0, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::HooksRan {
                        key: self.key,
                        records: cap_reached(records),
                    })
                    .await;
                self.inner.deliver(outcome).await;
            }
            None => {
                self.continuations.store(0, Ordering::Relaxed);
                self.inner.deliver(outcome).await;
            }
        }
    }
}

/// Why a hook in this batch set `continue: false`, if one did.
///
/// Reads the envelope rather than any outcome: `continue` is a common field, so
/// every seam that can act on a halt reads it the same way.
fn halt_reason(records: &[HookRecord]) -> Option<String> {
    records.iter().find_map(halt_of)
}

/// The same, restricted to the records the sink is the only route for.
fn tool_halt_reason(records: &[HookRecord]) -> Option<String> {
    records
        .iter()
        .filter(|r| is_tool_seam(&r.action))
        .find_map(halt_of)
}

/// One record's halt, with the fallback every seam shows when the hook set
/// `continue: false` without a `stopReason`.
fn halt_of(record: &HookRecord) -> Option<String> {
    record.halt.as_ref().map(|h| {
        h.reason
            .clone()
            .unwrap_or_else(|| "a hook set continue: false".to_string())
    })
}

/// Whether this record was made by a hook the runtime ran inline with a tool
/// call, rather than by one the server initiated.
///
/// Listed rather than `_`: a newly wired event must be classified here
/// deliberately, because a server event misfiled as a tool one is halted twice.
fn is_tool_seam(action: &HookAction) -> bool {
    match action {
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        | HookAction::PostToolBatch(_) => true,
        HookAction::SessionStart(_)
        | HookAction::SessionEnd(_)
        | HookAction::UserPromptSubmit(_)
        | HookAction::Stop(_)
        | HookAction::StopFailure(_)
        | HookAction::SubagentStart(_)
        | HookAction::SubagentStop(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_)
        | HookAction::Notification(_)
        | HookAction::CwdChanged(_) => false,
    }
}

/// Why a stop hook is holding this turn open, if one is.
///
/// Both stop events, because both mean the same thing: blocked *from stopping*,
/// so the agent that fired it continues under the same budget.
fn stop_verdict(records: &[HookRecord]) -> Option<String> {
    // An empty input is not a turn, so a hook that blocked without saying why
    // still has to say something.
    let said = |reason: &Option<String>, fallback: &str| {
        Some(reason.clone().unwrap_or_else(|| fallback.to_string()))
    };
    records.iter().find_map(|r| match &r.action {
        HookAction::Stop(s) => match &s.outcome {
            StopOutcome::Blocked(b) => said(&b.reason, "a Stop hook asked for another iteration"),
            // A failure is never fatal here: a stop hook runs after the fact, so
            // a guard that could not run cannot deny anything. Only `PreToolUse`
            // fails closed.
            StopOutcome::Ran(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
        },
        HookAction::SubagentStop(s) => match &s.outcome {
            SubagentStopOutcome::Blocked(b) => {
                said(&b.reason, "a SubagentStop hook asked for another iteration")
            }
            SubagentStopOutcome::Ran(_)
            | SubagentStopOutcome::Failed(_)
            | SubagentStopOutcome::CapReached(_) => None,
        },
        // Listed rather than `_`: a future event that can hold a turn open must
        // fail to compile here rather than be silently ignored.
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        | HookAction::PostToolBatch(_)
        | HookAction::SessionStart(_)
        | HookAction::SessionEnd(_)
        | HookAction::UserPromptSubmit(_)
        | HookAction::StopFailure(_)
        | HookAction::SubagentStart(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_)
        | HookAction::Notification(_)
        | HookAction::CwdChanged(_) => None,
    })
}

/// Narrow a blocking record's outcome to name the cap.
///
/// The only place `CapReached` is produced: `HookInvocation::record` sees one
/// hook's reply and cannot know the budget, so the outcome is narrowed here
/// rather than invented in the library.
fn cap_reached(mut records: Vec<HookRecord>) -> Vec<HookRecord> {
    for r in &mut records {
        match &mut r.action {
            HookAction::Stop(s) => {
                if let StopOutcome::Blocked(b) = &s.outcome {
                    s.outcome = StopOutcome::CapReached(b.clone());
                }
            }
            HookAction::SubagentStop(s) => {
                if let SubagentStopOutcome::Blocked(b) = &s.outcome {
                    s.outcome = SubagentStopOutcome::CapReached(b.clone());
                }
            }
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::SessionStart(_)
            | HookAction::SessionEnd(_)
            | HookAction::UserPromptSubmit(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            | HookAction::CwdChanged(_) => {}
        }
    }
    records
}

/// The interactive session's `AgentRunDef`.
fn session_run_def(settings: &AgentSettings) -> AgentRunDef {
    AgentRunDef {
        system_prompt: None,
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
        max_iterations: settings.max_iterations,
        max_retries: Some(settings.max_retries),
        allowed_tools: settings.allowed_tools.clone(),
    }
}

/// Wrap `base` with the memory tools and render the memory index.
async fn build_memory_layer(
    base: Arc<dyn Toolbox>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: &AgentSettings,
) -> Result<(Arc<dyn Toolbox>, String), String> {
    let spaces = &settings.memory_spaces;
    if spaces.is_empty() {
        return Ok((base, String::new()));
    }
    let Some(service) = memory else {
        tracing::warn!("session names memory spaces but no memory service is configured; ignoring");
        return Ok((base, String::new()));
    };
    let rows = service.memories_in(spaces).await?;
    let index = crate::memory::render_index(&rows, spaces);
    let toolbox: Arc<dyn Toolbox> = Arc::new(crate::memory::MemoryToolbox::new(
        base,
        service,
        spaces.clone(),
    ));
    Ok((toolbox, index))
}

/// Which of a session's agents a [`SessionContextProvider`] serves. The kind
/// decides the toolbox layers (session-metadata tools are main-only) and
/// whether preparation progress is broadcast (main-only — subagents are
/// quiet).
#[derive(Clone, Copy)]
enum SessionAgentKind {
    Main,
    Sub(Uuid),
    Step(Uuid),
}

impl SessionAgentKind {
    /// The key this agent is registered under on the session. One vocabulary:
    /// what the provider knows itself as is what the session looks it up by.
    fn agent_key(&self) -> AgentKey {
        match self {
            Self::Main => AgentKey::Main,
            Self::Sub(id) => AgentKey::Sub(*id),
            Self::Step(id) => AgentKey::Step(*id),
        }
    }
}

/// The runtime client an agent runs with. Subagents share the session's
/// sandbox but never its cwd/env bucket: the runtime keys that state by
/// agent id, so each subagent acts under its own identity.
fn scoped_client(kind: &SessionAgentKind, client: RuntimeClient) -> RuntimeClient {
    match kind {
        SessionAgentKind::Main => client,
        // Steps share the run's sandbox — that is the point — but never its
        // cwd/env bucket: the runtime keys that state by agent id, so each acts
        // under its own identity, exactly as a subagent does.
        SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) => {
            client.with_agent_id(id.to_string())
        }
    }
}

/// Appended to a subagent's system prompt: its place in the tree and how its
/// result travels. Deliberately short — the tools carry their own docs.
const SUBAGENT_PROMPT_SUFFIX: &str = "\n\n# Subagent role\n\
You are a subagent, spawned to work on one task. Your final message is your report: \
it is delivered to the agent that spawned you — make it self-contained. You may spawn \
your own subagents with spawn_agent and check on them with subagent_status. You cannot \
ask the user or rename the session; if you are blocked, report that instead.";

/// Appended to a workflow step's system prompt: what a step is, and that its
/// structured output is what decides where the run goes next. Deliberately
/// short — the `conclude` tool carries its own schema.
const STEP_PROMPT_SUFFIX: &str = "\n\n# Workflow step\n\
You are one step of a workflow, not a conversation. Your instruction and the previous \
step's result are in the message above. Finish by calling `conclude` — what you submit \
is both this step's result and what the workflow reads to decide which step runs next, \
so make it accurate and self-contained. You share one workspace with every other step: \
what you change on disk is what the next step sees. You may spawn subagents with \
spawn_agent. You cannot rename the session.";

/// Appended to an unattended session's system prompt (a routine run). It has
/// no `ask_user` tool, so the prompt says why rather than leaving the model to
/// discover a tool it was told about is missing.
const UNATTENDED_PROMPT_SUFFIX: &str = "\n\n# Unattended run\n\
This session was started by a routine, not by a person, and nobody is reading it while \
it runs. There is no ask_user tool: a question would park the run with nobody to answer \
it. Work from the instructions you were given — where they leave a choice open, make the \
reasonable one, say which you made and why, and carry on. Your final message is the \
report; make it self-contained.";

/// Per-run context for a session's agent, resolved on the run's own task.
///
/// It asks the [`RuntimeClientProvider`] for a client each run rather than
/// holding one: that is what lets the agent be resident across a hibernate and
/// resume without knowing either happened.
struct SessionContextProvider {
    runtimes: RuntimeClientProvider,
    registry: crate::sessions::spec::SharedProviderRegistry,
    mcp: Option<Arc<crate::mcp::McpService>>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: AgentSettings,
    /// A workflow step's declared output schema, which becomes the input
    /// schema of its `conclude` tool. `None` for every other kind of agent.
    step_output_schema: Option<Value>,
    session_id: Uuid,
    kind: SessionAgentKind,
    /// The plugin-declared agent type this agent runs as, for a subagent that
    /// was spawned with one. The *name* only — the definition is resolved from
    /// the library scan on every `provide()`, so a subagent that outlives its
    /// plugin fails rather than running a prompt nobody can point at.
    agent_type: Option<String>,
    /// Whether nobody is watching this session (a routine run). Decides one
    /// thing: the main agent gets no `ask_user`, and is told why.
    unattended: bool,
    /// The owning session's mailbox — routes the server-owned tools.
    session: ActorRef<SessionCommand>,
    frames: broadcast::Sender<SessionFrame>,
    /// The client the most recent `provide()` resolved. Cheap to keep — cloning
    /// shares the same in-flight-call tracking — and it is what lets
    /// [`SessionActor::cancel_run`] cancel without a fresh vendor round-trip.
    last_client: Mutex<Option<RuntimeClient>>,
}

impl SessionContextProvider {
    /// The provider for one named model, or `None` when horsie has none.
    ///
    /// Separate from [`Self::llm_provider`] because a missing model means two
    /// different things: the session's own model is a failure, while a
    /// plugin agent's is a declaration horsie cannot honour and inherits past.
    fn provider_for(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        self.registry.read().ok()?.get(model).cloned()
    }

    fn llm_provider(&self) -> Result<Arc<dyn LlmProvider>, String> {
        let reg = self
            .registry
            .read()
            .map_err(|_| "provider registry lock poisoned".to_string())?;
        reg.get(&self.settings.model)
            .cloned()
            .ok_or_else(|| format!("no provider registered for model '{}'", self.settings.model))
    }

    /// The client the run currently in flight already acquired, if any.
    fn cached_client(&self) -> Option<RuntimeClient> {
        self.last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether this agent loads the shared plugin library — and so whether any
    /// hook could possibly be declared for it.
    fn use_plugins(&self) -> bool {
        self.settings.use_plugins.unwrap_or(true)
    }

    /// Acquire this agent's runtime handle, scoped to it. Sink-less: `provide`
    /// attaches one for the tool hooks that report themselves mid-call, while
    /// `start_hooks` returns its records to the agent, which journals them
    /// itself. A sink there would both duplicate them and race the turn they
    /// must precede.
    async fn runtime_client(&self) -> Result<RuntimeClient, ContextError> {
        let client = self.runtimes.get().await.map_err(|e| match e {
            // The one failure the session can never retry: the vendor is alive
            // and says the runtime is gone. A vendor that is merely offline
            // (`Unavailable`) says nothing about the runtime's existence.
            RuntimeError::Gone(m) => ContextError::terminal(m),
            other @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)) => {
                ContextError::retryable(other.to_string())
            }
        })?;
        Ok(scoped_client(&self.kind, client))
    }

    /// The `agent_type` a `SubagentStart` / `SubagentStop` hook matches on.
    ///
    /// The plugin-declared type when the spawn named one, so a hook may select
    /// `reviewer` and fire for reviewers only. An untyped spawn reports
    /// `"subagent"` — the general-purpose case, which is a kind and not a lie.
    fn agent_type(&self) -> String {
        self.agent_type
            .clone()
            .unwrap_or_else(|| "subagent".to_string())
    }
}

#[async_trait]
impl ContextProvider for SessionContextProvider {
    fn has_start_hooks(&self) -> bool {
        self.use_plugins()
    }

    /// Fire this turn's start hooks, before the run snapshots its history.
    ///
    /// A hook that cannot run is not a turn that cannot start: `run_hooks`
    /// failures fall back to no records, exactly as the `SessionStart` bootstrap
    /// did. Acquiring the runtime is the only step that can fail the turn, and
    /// it fails it the same way `provide` would have, one step later.
    async fn start_hooks(&self, turn: StartTurn) -> Result<Vec<HookRecord>, ContextError> {
        // Reuse the handle the last run resolved when there is one, so a warm
        // agent pays one vendor round-trip per turn rather than two. Only the
        // first turn of a load has nothing cached — and that is the turn whose
        // hooks could not have run any earlier anyway.
        let client = match self.cached_client() {
            Some(cached) => cached.without_hook_sink(),
            None => self.runtime_client().await?,
        };
        let mut records = Vec::new();
        if let Some(source) = turn.start_source {
            // A subagent's start is a `SubagentStart`. It used to be a
            // `SessionStart`, because this call was not gated on the kind at
            // all — a subagent is not a session, and the two events carry
            // different matcher domains.
            let event = match self.kind {
                SessionAgentKind::Sub(id) => ServerHookEvent::SubagentStart(SubagentStartInput {
                    agent_id: id.to_string(),
                    agent_type: self.agent_type(),
                }),
                SessionAgentKind::Main | SessionAgentKind::Step(_) => {
                    ServerHookEvent::SessionStart(SessionStartInput { source })
                }
            };
            records.extend(client.run_hooks(event).await.unwrap_or_default());
        }
        if let Some(prompt) = turn.prompt {
            records.extend(
                client
                    .run_hooks(ServerHookEvent::UserPromptSubmit(UserPromptSubmitInput {
                        prompt,
                    }))
                    .await
                    .unwrap_or_default(),
            );
        }
        Ok(records)
    }

    async fn provide(&self) -> Result<Contexts, ContextError> {
        let settings = &self.settings;
        let mut def = session_run_def(settings);
        let use_plugins = settings.use_plugins.unwrap_or(true);
        // Preparation progress is main-only: subagents are quiet by design.
        let broadcast = matches!(
            self.kind,
            SessionAgentKind::Main | SessionAgentKind::Step(_)
        );

        if broadcast {
            emit_progress(&self.frames, "acquiring_runtime", None);
        }
        let runtime_client = self.runtime_client().await?;
        // Hooks run runtime-side and report what they did on the tool response.
        // Routing those records here is what makes a plugin's interventions
        // visible to the user rather than silent.
        let runtime_client = runtime_client.with_hook_sink(Arc::new(SessionHookSink {
            target: self.session.clone(),
            key: self.kind.agent_key(),
        }));
        // Cached *after* the sink is attached, not before: `Stop` runs its hooks
        // through this handle once the turn is over, and a sink-less clone would
        // run them and drop every record on the floor. Cancellation is
        // unaffected — in-flight tracking is shared across clones.
        *self
            .last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(runtime_client.clone());

        if broadcast {
            emit_progress(&self.frames, "scanning_workspace", None);
        }
        let (ws, shared_scan) = scan_workspace(&runtime_client, None, use_plugins).await;
        // No `SessionStart` here any more. It used to fire on this line, once
        // per *run* — `provide` is per-run — so every turn re-ran every start
        // hook, always reporting `source: "startup"`. It now fires once per
        // agent load at `start_hooks`, early enough for its context to reach the
        // turn that triggered it.
        let shared = use_plugins.then(|| SharedContext {
            skills: Arc::new(shared_scan.skills),
            agents: Arc::new(shared_scan.agents),
            root: shared_scan.root,
        });
        // Resolved here rather than carried from the spawn: the definition is a
        // property of the library as it is *now*, so an agent whose plugin was
        // uninstalled between spawn and wake fails loudly.
        let plugin_agent = match (&self.agent_type, shared.as_ref()) {
            (None, _) => None,
            (Some(name), Some(shared)) => Some(shared.agents.get(name).cloned().ok_or_else(|| {
                ContextError::retryable(format!(
                    "this subagent runs as agent type '{name}', which no installed plugin declares"
                ))
            })?),
            (Some(name), None) => {
                return Err(ContextError::retryable(format!(
                    "this subagent runs as agent type '{name}', but the session loads no plugins"
                )));
            }
        };
        if let Some(agent) = &plugin_agent
            && !agent.def.tools.is_empty()
        {
            // The declared allowlist is in Claude's vocabulary; horsie's filter
            // is in horsie's. Same table the hook matchers use, read backwards.
            let allowed: Vec<String> = agent
                .def
                .tools
                .iter()
                .flat_map(|t| horsie_support::plugin::hooks::horsie_tools_for(t))
                .map(str::to_string)
                .collect();
            if allowed.is_empty() {
                tracing::warn!(
                    agent = %agent.def.name,
                    declared = ?agent.def.tools,
                    "agent's tool allowlist names no tool horsie has; it will run with none"
                );
            }
            def.allowed_tools = Some(allowed);
        }
        // A declared `model` is honoured only when horsie actually has it.
        // Every model declared in the wild is an alias (`inherit`, `sonnet`,
        // `opus`), and mapping those onto whatever the catalogue holds would let
        // a plugin author switch a kimi session to Anthropic by writing a word
        // in a file.
        let provider = match plugin_agent.as_ref().and_then(|a| a.def.model.as_deref()) {
            Some(model) => match self.provider_for(model) {
                Some(provider) => provider,
                None => {
                    tracing::info!(
                        model,
                        "agent declares a model horsie has no provider for; inheriting the session's"
                    );
                    self.llm_provider()?
                }
            },
            None => self.llm_provider()?,
        };
        let mcp: Vec<Arc<dyn Toolbox>> = if settings.mcp_servers.is_empty() {
            Vec::new()
        } else if let Some(mcp_svc) = self.mcp.as_ref() {
            if broadcast {
                emit_progress(&self.frames, "connecting_tools", None);
            }
            mcp_svc
                .toolboxes_for(&settings.mcp_servers)
                .await
                .map_err(|e| format!("build MCP toolboxes: {e}"))?
        } else {
            tracing::warn!(
                session = %self.session_id,
                "session names MCP servers but no MCP service is configured; ignoring"
            );
            Vec::new()
        };
        let base: Arc<dyn Toolbox> = DefaultToolboxFactory.for_agent(
            &def,
            runtime_client.clone(),
            ws.names(),
            use_plugins,
            mcp,
        );
        let (with_memory, memory_index) =
            build_memory_layer(base, self.memory.clone(), settings).await?;
        let caller = match self.kind {
            // A step roots its own tree, so its spawns are that tree's `Main`.
            SessionAgentKind::Main | SessionAgentKind::Step(_) => SubAgentParent::Main,
            SessionAgentKind::Sub(id) => SubAgentParent::SubAgent(id),
        };
        // A zero cap disables subagents outright: no tools advertised, so the
        // model never meets a tool that only ever rejects.
        let with_spawn: Arc<dyn Toolbox> = if settings.max_subagents() == 0 {
            with_memory
        } else {
            Arc::new(SubAgentToolbox::new(
                with_memory,
                self.session.clone(),
                caller,
                shared
                    .as_ref()
                    .map(|s| Arc::clone(&s.agents))
                    .unwrap_or_default(),
            ))
        };
        let toolbox: Arc<dyn Toolbox> = match self.kind {
            // An unattended session skips the ask layer entirely rather than
            // offering a tool whose answer would never come.
            SessionAgentKind::Main if self.unattended => {
                Arc::new(SessionTitleToolbox::new(with_spawn, self.session.clone()))
            }
            SessionAgentKind::Main => {
                let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_spawn));
                Arc::new(SessionTitleToolbox::new(inner, self.session.clone()))
            }
            // A step gets `conclude` instead of the ask and title layers: it
            // asks through `conclude(kind=ask)`, and its title belongs to the
            // run rather than to one step.
            SessionAgentKind::Step(_) => crate::sessions::workflow::StepConcludeToolbox::wrap(
                with_spawn,
                self.step_output_schema.as_ref(),
                // Same rule as the run def: asking rides on `conclude`, which
                // only a step with a declared output has.
                self.step_output_schema.is_some() && !self.unattended,
            ),
            SessionAgentKind::Sub(_) => with_spawn,
        };
        let system_prompt = compose_system_prompt(Some(SESSION_AGENT_PROMPT), &ws, shared.as_ref());
        // A typed subagent's role section is its plugin's prompt. The workspace
        // and skill sections around it are untouched: a named agent still works
        // in the same workspace, with the same skills.
        let subagent_role = plugin_agent
            .as_ref()
            .map(|a| format!("\n\n# Subagent role: {}\n\n{}\n", a.def.name, a.def.prompt));
        let suffix: Option<&str> = match &self.kind {
            SessionAgentKind::Main if self.unattended => Some(UNATTENDED_PROMPT_SUFFIX),
            SessionAgentKind::Main => None,
            SessionAgentKind::Step(_) => Some(STEP_PROMPT_SUFFIX),
            SessionAgentKind::Sub(_) => {
                Some(subagent_role.as_deref().unwrap_or(SUBAGENT_PROMPT_SUFFIX))
            }
        };
        let system_prompt = match suffix {
            None => system_prompt,
            Some(suffix) => Some(match system_prompt {
                Some(p) => format!("{p}{suffix}"),
                None => suffix.trim_start().to_string(),
            }),
        };
        let system_prompt = match (system_prompt, memory_index.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{memory_index}")),
            (Some(p), true) => Some(p),
            (None, false) => Some(memory_index),
            (None, true) => None,
        };
        if broadcast {
            emit_progress(&self.frames, "ready", None);
        }
        Ok(Contexts {
            provider,
            toolbox,
            system_prompt,
        })
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
            SessionDomainEvent::MessageQueued { id, text, at_ms } => {
                state.inbox.push(InboxMessage { id, text, at_ms });
            }
            SessionDomainEvent::TurnBegan { consumed, .. } => {
                state.status = SessionStatus::Running;
                state.inbox.retain(|m| !consumed.contains(&m.id));
                // A turn beginning ends the park either way: the asks were
                // answered, or the user moved on and they were abandoned. Both
                // record a result for every call before the turn starts.
                state.pending_asks.clear();
                // The previous turn's failure is history once a new turn is
                // under way; leaving it set makes the detail endpoint report a
                // stale error for the rest of the session's life.
                state.last_error = None;
            }
            SessionDomainEvent::AskRecorded {
                tool_call_id,
                question,
                ..
            } => {
                state.pending_asks.push(PendingAsk {
                    tool_call_id,
                    question,
                });
                state.status = SessionStatus::AwaitingInput {
                    asks: state.pending_asks.clone(),
                };
                if let Some(run) = state.mode.run_mut() {
                    run.apply_awaiting();
                }
            }
            SessionDomainEvent::TurnEnded { .. }
            | SessionDomainEvent::TurnStopped { .. }
            | SessionDomainEvent::TurnInterrupted { .. } => {
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::TurnFailed { error, .. } => {
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::SessionFailed { reason, .. } => {
                state.status = SessionStatus::Unrecoverable {
                    reason: reason.clone(),
                };
                state.last_error = Some(reason);
            }
            SessionDomainEvent::UsageRecorded {
                agent_id,
                usage_total,
                ..
            } => {
                state.agent_usage.insert(agent_id, usage_total);
            }
            SessionDomainEvent::SubAgentSpawned {
                id,
                parent,
                label,
                task,
                depth,
                at_ms,
                agent_type,
            } => {
                if let Some(tree) = state.mode.tree_of_parent_mut(parent) {
                    tree.apply_spawned(id, parent, label, task, depth, at_ms, agent_type);
                }
            }
            SessionDomainEvent::SubAgentRunning { id, at_ms } => {
                if let Some(tree) = state.mode.tree_of_node_mut(id) {
                    tree.apply_running(id, at_ms);
                }
            }
            SessionDomainEvent::SubAgentCompleted { id, output, at_ms } => {
                if let Some(tree) = state.mode.tree_of_node_mut(id) {
                    tree.apply_completed(id, output, at_ms);
                }
            }
            SessionDomainEvent::SubAgentFailed { id, error, at_ms } => {
                if let Some(tree) = state.mode.tree_of_node_mut(id) {
                    tree.apply_failed(id, error, at_ms);
                }
            }
            SessionDomainEvent::SubAgentNotified { id, .. } => {
                if let Some(tree) = state.mode.tree_of_node_mut(id) {
                    tree.apply_notified(id);
                }
            }
            SessionDomainEvent::StepStarted {
                at_ms,
                step,
                agent,
                attempt,
                from,
                via,
                input,
                ..
            } => {
                // The first step is what turns the state into a run:
                // `initial_state` is static and cannot see the spec, so the mode
                // is established by the log rather than at construction.
                if !state.mode.is_workflow() {
                    state.mode = SessionModeState::Workflow(Default::default());
                }
                if let Some(run) = state.mode.run_mut() {
                    run.apply_started(step, agent, attempt, from, via, input, at_ms);
                }
                state.status = SessionStatus::Running;
                state.last_error = None;
            }
            SessionDomainEvent::StepConcluded {
                at_ms,
                index,
                output,
            } => {
                if let Some(run) = state.mode.run_mut() {
                    run.apply_concluded(index, output, at_ms);
                }
            }
            SessionDomainEvent::StepFailed {
                at_ms,
                index,
                error,
            } => {
                if let Some(run) = state.mode.run_mut() {
                    run.apply_step_failed(index, error.clone(), at_ms);
                    run.apply_failed(error.clone());
                }
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::StepCancelled { at_ms, index } => {
                if let Some(run) = state.mode.run_mut() {
                    run.apply_cancelled(index, at_ms);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFinished { output, .. } => {
                if let Some(run) = state.mode.run_mut() {
                    run.apply_finished(output);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFailed { error, .. } => {
                if let Some(run) = state.mode.run_mut() {
                    run.apply_failed(error.clone());
                }
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
        }
        state
    }

    /// Announce a changed agent roster once the change is durable. The frame
    /// carries no payload — the roster is a current value, so a client re-reads
    /// the session document rather than accumulating deltas.
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], _state: &SessionState) {
        let tree_changed = events.iter().any(|e| {
            matches!(
                e,
                SessionDomainEvent::SubAgentSpawned { .. }
                    | SessionDomainEvent::SubAgentRunning { .. }
                    | SessionDomainEvent::SubAgentCompleted { .. }
                    | SessionDomainEvent::SubAgentFailed { .. }
            )
        });
        if tree_changed {
            let _ = self.frames.send(SessionFrame::AgentTreeChanged);
        }
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SessionCommand::UserMessage { text, reply } => {
                if let Err(why) = self.orchestrator.accepts(SessionCommandKind::UserMessage) {
                    let _ = reply.send(Err(UserMessageError::Rejected(why.to_string())));
                    return CommandEffect::none();
                }
                self.on_user_message(state, text, reply, ctx).await
            }
            SessionCommand::Stop { reply } => {
                if state.status != SessionStatus::Running {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                self.cancel_run().await;
                let _ = reply.send(());
                self.report(SessionStatus::Idle).await;
                let mut events = vec![SessionDomainEvent::TurnStopped { at_ms: now_ms() }];
                // Stop is a turn boundary like any other, so anything the user
                // queued while the cancelled turn ran starts the next one.
                let next = Self::apply_event(
                    state.clone(),
                    SessionDomainEvent::TurnStopped { at_ms: now_ms() },
                );
                events.extend(self.flush_then_drain(&next, ctx).await);
                self.publish_inbox(state, &events);
                CommandEffect::persist(events)
            }
            SessionCommand::Delete { reply } => {
                self.cancel_run().await;
                self.stop_agents().await;
                self.deps
                    .runtimes
                    .delete(&self.id.to_string(), &self.spec.vendor)
                    .await;
                let _ = reply.send(());
                CommandEffect::stop()
            }
            SessionCommand::History {
                agent_id,
                query,
                reply,
            } => {
                // Read history from the resident actor's in-memory state. No
                // journal access, no runtime — opening a session to read it
                // stays free of sandbox cost.
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let page = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::GetHistory { query, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            SessionCommand::UsageStats { reply } => {
                let stats = self.read_usage(state).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
            SessionCommand::AgentState { agent_id, reply } => {
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let view = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::GetState { reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(view);
                CommandEffect::none()
            }
            SessionCommand::SubscribeAgent { agent_id, reply } => {
                // Resolving first means subscribing to a cold subagent spawns
                // it, so a watcher sees its output the moment it wakes.
                let key = self
                    .resolve_agent(state, ctx, agent_id.as_deref())
                    .map(|(key, _)| key);
                let rx = key.map(|key| self.agent_frames(key).subscribe());
                let _ = reply.send(rx);
                CommandEffect::none()
            }
            SessionCommand::Answer { answers, reply } => {
                self.on_answer(state, answers, reply).await
            }
            SessionCommand::Snapshot { reply } => {
                let _ = reply.send(SessionSnapshot {
                    status: state.status.clone(),
                    inbox: state.inbox.clone(),
                });
                CommandEffect::none()
            }
            SessionCommand::PrepareOffload { reply } => {
                // A run started while the supervisor was deciding: refuse, and
                // let the idle clock start again. This is the invariant that
                // keeps a forty-minute tool call from being unloaded out from
                // under itself — the main agent's run, or any subagent's.
                if state.status == SessionStatus::Running || state.mode.subagents().has_active() {
                    let _ = reply.send(false);
                    return CommandEffect::none();
                }
                self.stop_agents().await;
                self.deps
                    .runtimes
                    .hibernate(&self.id.to_string(), &self.spec.vendor)
                    .await;
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            SessionCommand::AgentOutcome(outcome) => {
                self.on_agent_outcome(state, outcome, ctx).await
            }
            SessionCommand::RunState { reply } => {
                let _ = reply.send(state.mode.run().cloned());
                CommandEffect::none()
            }
            SessionCommand::AdvanceRun => {
                CommandEffect::persist(self.flush_then_drain(state, ctx).await)
            }
            SessionCommand::RetryStep { index, reply } => {
                self.on_retry_step(state, index, reply, ctx).await
            }
            SessionCommand::HooksRan { key, records } => {
                // The agent owns its own transcript, so the records go to it
                // rather than into the session's log. An agent that has already
                // gone is not an error: the records describe a call it made
                // before it left, and there is nothing left to tell.
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.get(key)) {
                    let _ = agent.tell(AgentCommand::HooksRan { records }).await;
                }
                CommandEffect::none()
            }
            SessionCommand::HaltAgent { key, reason } => {
                // A halt races the turn it is halting: the records reach the
                // session on the sink while the tool call that produced them is
                // still returning, so the turn can finish first. Failing it then
                // would rewrite a turn that already ended — which is why
                // `ContinueAfterStop` below no-ops on the same condition.
                let live = self
                    .agents
                    .as_ref()
                    .and_then(|a| a.get(key))
                    .filter(|_| state.status == SessionStatus::Running)
                    .cloned();
                let Some(agent) = live else {
                    tracing::warn!(
                        session = %self.id,
                        "a hook halted an agent whose turn had already ended; ignored"
                    );
                    return CommandEffect::none();
                };
                // Cancel first, so the agent is not still appending to its own
                // journal when the outcome below is folded.
                let (tx, rx) = oneshot::channel();
                let _ = agent.tell(AgentCommand::Cancel { ack: Some(tx) }).await;
                if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
                    tracing::warn!(session = %self.id, "halted agent did not finish in time");
                }
                // Routed through the ordinary outcome path rather than given its
                // own per-key branching: a halt is a failure with a reason, and
                // what a failure means for a main agent, a subagent and a step is
                // already decided in one place.
                self.on_agent_outcome(
                    state,
                    AgentOutcome::Failed {
                        session_id: match key {
                            AgentKey::Main => self.id,
                            AgentKey::Sub(id) | AgentKey::Step(id) => id,
                        },
                        error: reason,
                        // Not recoverable and not terminal: re-running the same
                        // turn would meet the same hook, but the session is
                        // perfectly able to run the next thing the user sends.
                        recoverable: false,
                        terminal: false,
                    },
                    ctx,
                )
                .await
            }
            SessionCommand::ContinueAfterStop { key, reason } => {
                // The same path recovery uses to continue an interrupted task:
                // a plain user-message turn whose input is the hook's reason.
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.get(key)) {
                    let _ = agent
                        .tell(AgentCommand::Resume {
                            results: Vec::new(),
                            message: Some(reason),
                            subagent_results: Vec::new(),
                        })
                        .await;
                }
                CommandEffect::none()
            }
            SessionCommand::SubAgentTree { reply } => {
                let tree = state
                    .mode
                    .subagents()
                    .ids()
                    .into_iter()
                    .filter_map(|id| state.mode.subagents().get(&id).map(|rec| (id, rec.clone())))
                    .collect();
                let _ = reply.send(tree);
                CommandEffect::none()
            }
            SessionCommand::ReconcileSubAgents => {
                let interrupted = state.mode.subagents().interrupted();
                if interrupted.is_empty() {
                    return CommandEffect::none();
                }
                CommandEffect::persist(
                    interrupted
                        .into_iter()
                        .map(|id| SessionDomainEvent::SubAgentFailed {
                            at_ms: now_ms(),
                            id,
                            error: INTERRUPTED_ERROR.to_string(),
                        })
                        .collect(),
                )
            }
            SessionCommand::ReconcileInterrupted => {
                if state.status == SessionStatus::Running {
                    self.report(SessionStatus::Idle).await;
                    CommandEffect::persist(vec![SessionDomainEvent::TurnInterrupted {
                        at_ms: now_ms(),
                    }])
                } else {
                    CommandEffect::none()
                }
            }
            SessionCommand::SetSessionTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => self.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
                CommandEffect::none()
            }
            SessionCommand::SpawnSubAgent {
                caller,
                label,
                task,
                agent_type,
                reply,
            } => {
                let Some(parent_depth) = state.mode.subagents().depth_of(caller) else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                if parent_depth >= MAX_SUBAGENT_DEPTH {
                    let _ = reply.send(Err(format!(
                        "max subagent depth {MAX_SUBAGENT_DEPTH} reached"
                    )));
                    return CommandEffect::none();
                }
                let max = self.spec.agent.max_subagents();
                if state.mode.subagents().active_count() >= max {
                    let _ = reply.send(Err(format!("{max} subagents already active")));
                    return CommandEffect::none();
                }
                // Persist first, spawn second: a crash between the two replays
                // as a Running node with no actor, which recovery reconciles
                // to failed — never an untracked agent.
                let id = Uuid::new_v4();
                let spawned = SessionDomainEvent::SubAgentSpawned {
                    at_ms: now_ms(),
                    id,
                    parent: caller,
                    label: label.clone(),
                    task: task.clone(),
                    depth: parent_depth + 1,
                    agent_type: agent_type.clone(),
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = ctx.self_ref();
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "spawn ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::FinishSpawnSubAgent {
                            id,
                            label,
                            task,
                            agent_type,
                            reply,
                            persisted,
                        })
                        .await;
                });
                CommandEffect::persist(vec![spawned]).and_ack(tx)
            }
            SessionCommand::FinishSpawnSubAgent {
                id,
                label,
                task,
                agent_type,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist subagent: {e}")));
                    return CommandEffect::none();
                }
                let agent = self.spawn_sub_agent_actor(ctx, id, agent_type);
                let _ = agent
                    .tell(AgentCommand::Resume {
                        results: Vec::new(),
                        message: Some(task),
                        subagent_results: Vec::new(),
                    })
                    .await;
                emit_progress(
                    &self.frames,
                    "subagent_spawned",
                    Some(format!("\"{label}\" ({id})")),
                );
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SessionCommand::SubAgentStatus { caller, id, reply } => {
                let rendered = match id {
                    Some(id) if state.mode.subagents().visible_to(caller, &id) => state
                        .mode
                        .subagents()
                        .render_node(&id)
                        .ok_or_else(|| format!("no such subagent: {id}")),
                    // Out-of-subtree and unknown ids are indistinguishable —
                    // neither confirms the node exists.
                    Some(id) => Err(format!("no such subagent: {id}")),
                    None => Ok(state.mode.subagents().render_subtree(caller)),
                };
                let _ = reply.send(rendered);
                CommandEffect::none()
            }
        }
    }

    /// Loading a session spawns its agent and repairs a turn the process died
    /// in. It calls no vendor, starts no run, and drains nothing — an
    /// interrupted assistant turn is over, and queued user messages wait for
    /// the next turn the user starts.
    async fn on_recovery_complete(&mut self, state: &SessionState, ctx: &mut ActorContext<Self>) {
        if self.spec.workflow.is_some() {
            // A run has no main agent. Step actors, like subagent actors, stay
            // cold: they spawn on demand for a history read, a retry, or the
            // next step the orchestrator picks.
            self.agents = Some(SessionAgents::workflow());
            // The one place a session starts work at load, and only for a run
            // that has never started a step: a workflow is created and then
            // left to begin by itself, with no first message to trigger it. A
            // run whose log is empty still reads as `Interactive` here — the
            // mode is established by the first `StepStarted` — so an absent run
            // means "never started", exactly like `Pending`. A run that is
            // Running, Suspended or terminal is left alone.
            let unstarted = match state.mode.run() {
                None => true,
                Some(run) => run.status == crate::sessions::workflow::WorkflowRunStatus::Pending,
            };
            if unstarted {
                let _ = ctx.self_ref().tell(SessionCommand::AdvanceRun).await;
            }
        } else {
            self.spawn_main_agent(ctx);
        }
        // Subagent actors stay cold: a session that spawned hundreds of them
        // must not replay hundreds of journals on open. They spawn lazily —
        // on a history read or an owed-result flush — and recovery starts no
        // runs either way.
        if !state.mode.subagents().interrupted().is_empty() {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileSubAgents)
                .await;
        }
        if state.status == SessionStatus::Running {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileInterrupted)
                .await;
            return;
        }
        // Loading is not a transition, but it is the first moment anyone can
        // learn this status: the supervisor's cache is empty until a session
        // reports, and a page already watching hears nothing otherwise.
        self.report(state.status.clone()).await;
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

    fn queued(id: &str, text: &str) -> SessionDomainEvent {
        SessionDomainEvent::MessageQueued {
            id: id.to_string(),
            text: text.to_string(),
            at_ms: 0,
        }
    }

    use crate::sessions::orchestrator::MERGE_SEPARATOR;

    fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
        events
            .into_iter()
            .fold(SessionState::default(), SessionActor::apply_event)
    }

    /// What this actor's orchestrator decides for a state. `drain` used to be a
    /// method here; the decision moved to the orchestrator and the actor only
    /// performs it, so these tests assert on the decision.
    fn decisions(actor: &SessionActor, state: &SessionState) -> Vec<AgentAction> {
        actor.orchestrator.next_actions(state)
    }

    #[test]
    fn a_fresh_session_is_idle_with_an_empty_inbox() {
        let s = SessionState::default();
        assert_eq!(s.status, SessionStatus::Idle);
        assert!(s.inbox.is_empty());
    }

    #[test]
    fn queued_messages_accumulate_without_changing_status() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        assert_eq!(s.status, SessionStatus::Idle, "queueing is not running");
        assert_eq!(s.inbox.len(), 2);
    }

    #[test]
    fn a_turn_consumes_exactly_the_messages_it_names() {
        let s = fold(vec![
            queued("m1", "one"),
            queued("m2", "two"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m3", "three"),
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        let ids: Vec<&str> = s.inbox.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["m2", "m3"],
            "a message that arrived after the turn began must still be owed an answer"
        );
    }

    #[test]
    fn a_turn_that_answers_an_ask_clears_it() {
        // `answering` is how turns before multi-ask recorded it; a journal
        // written then must still fold to the same place.
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: Some("call-1".into()),
                answered: Vec::new(),
            },
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        assert!(s.pending_asks.is_empty(), "the ask was answered");
    }

    #[test]
    fn two_asks_in_one_turn_are_both_pending_until_a_turn_begins() {
        let asked = |id: &str, q: &str| SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some(id.to_string()),
            question: q.to_string(),
        };
        let s = fold(vec![
            asked("call-1", "which branch?"),
            asked("call-2", "which model?"),
        ]);
        let SessionStatus::AwaitingInput { asks } = &s.status else {
            panic!("expected AwaitingInput, got {:?}", s.status);
        };
        assert_eq!(asks.len(), 2, "the status carries what must be answered");
        assert_eq!(asks[0].question, "which branch?");
        assert_eq!(asks[1].question, "which model?");
        assert_eq!(s.pending_asks.len(), 2);

        // Answered together, or abandoned together — either way the turn that
        // begins is the end of the park.
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: Vec::new(),
                answering: None,
                answered: vec!["call-1".into(), "call-2".into()],
            },
        );
        assert_eq!(s.status, SessionStatus::Running);
        assert!(s.pending_asks.is_empty());
    }

    #[test]
    fn an_ask_survives_a_crash_so_the_answer_is_not_re_asked() {
        // TurnBegan is what clears the ask, and it is journaled with the
        // consumption in one step: a crash before it replays to "still asking".
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
        ]);
        assert!(matches!(s.status, SessionStatus::AwaitingInput { .. }));
        assert_eq!(
            s.pending_asks
                .first()
                .and_then(|a| a.tool_call_id.as_deref()),
            Some("call-1")
        );
        assert_eq!(s.inbox.len(), 1, "the answer is still owed");
    }

    #[test]
    fn stop_and_interrupt_both_land_idle_and_keep_the_inbox() {
        for boundary in [
            SessionDomainEvent::TurnStopped { at_ms: 0 },
            SessionDomainEvent::TurnInterrupted { at_ms: 0 },
        ] {
            let s = fold(vec![
                queued("m1", "one"),
                SessionDomainEvent::TurnBegan {
                    at_ms: 0,
                    consumed: vec!["m1".into()],
                    answering: None,
                    answered: Vec::new(),
                },
                queued("m2", "queued while running"),
                boundary,
            ]);
            assert_eq!(s.status, SessionStatus::Idle);
            assert_eq!(
                s.inbox.len(),
                1,
                "an accepted message is a promise; a stop cancels the turn, not the promise"
            );
        }
    }

    #[test]
    fn a_failed_turn_is_sticky_but_not_terminal() {
        let s = fold(vec![
            queued("m1", "still owed an answer"),
            SessionDomainEvent::TurnFailed {
                at_ms: 0,
                error: "provider exploded".into(),
            },
        ]);
        assert!(matches!(s.status, SessionStatus::Failed { .. }));
        assert_eq!(s.last_error.as_deref(), Some("provider exploded"));
        assert_eq!(
            s.inbox.len(),
            1,
            "a turn that failed answered nothing; the queue is still owed"
        );

        // The next turn moves it straight back to Running.
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec![],
                answering: None,
                answered: Vec::new(),
            },
        );
        assert_eq!(s.status, SessionStatus::Running);
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn a_gone_runtime_is_terminal() {
        let s = fold(vec![SessionDomainEvent::SessionFailed {
            at_ms: 0,
            reason: "vendor has no runtime".into(),
        }]);
        assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
    }

    #[test]
    fn usage_is_recorded_per_agent() {
        let s = fold(vec![SessionDomainEvent::UsageRecorded {
            at_ms: 0,
            agent_id: MAIN_AGENT_ID.to_string(),
            usage_total: UsageTotal {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            },
        }]);
        assert_eq!(s.agent_usage.get(MAIN_AGENT_ID).unwrap().input_tokens, 10);
    }

    #[test]
    fn subagent_events_fold_into_the_tree() {
        use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id,
            parent: SubAgentParent::Main,
            label: "research".into(),
            task: "look into it".into(),
            depth: 1,
            agent_type: None,
        }]);
        assert_eq!(s.mode.subagents().active_count(), 1);

        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentCompleted {
                at_ms: 0,
                id,
                output: "answer".into(),
            },
        );
        let rec = s.mode.subagents().get(&id).unwrap();
        assert_eq!(rec.status, SubAgentStatus::Completed);
        assert!(!rec.notified);

        let s = SessionActor::apply_event(s, SessionDomainEvent::SubAgentNotified { at_ms: 0, id });
        assert!(s.mode.subagents().get(&id).unwrap().notified);
    }

    #[test]
    fn a_running_then_failed_subagent_reads_as_interrupted_then_terminal() {
        use crate::sessions::subagents::SubAgentParent;
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id,
            parent: SubAgentParent::Main,
            label: "w".into(),
            task: "t".into(),
            depth: 1,
            agent_type: None,
        }]);
        assert_eq!(s.mode.subagents().interrupted(), vec![id]);
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentFailed {
                at_ms: 0,
                id,
                error: "interrupted by restart".into(),
            },
        );
        assert!(s.mode.subagents().interrupted().is_empty());
    }

    #[test]
    fn merging_joins_in_arrival_order_with_a_blank_line() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let merged = s
            .inbox
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(MERGE_SEPARATOR);
        assert_eq!(merged, "one\n\ntwo");
    }

    #[test]
    fn a_title_is_derived_from_the_first_line_only() {
        assert_eq!(derive_title("hello\nworld").as_deref(), Some("hello"));
        assert!(derive_title("   \n").is_none());
        let long = "x".repeat(TITLE_MAX_CHARS + 10);
        let title = derive_title(&long).unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
    }

    // ── Actor-level coverage: `drain()` and `PrepareOffload`'s refuse-if-running
    // branch. The rewrite that introduced the durable inbox dropped both.

    fn actor_spec_fixture() -> SessionSpec {
        use crate::sessions::spec::WorkspaceDef;
        SessionSpec {
            name: Some("test".into()),
            agent: AgentSettings {
                model: "mock".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
            },
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            workflow: None,
        }
    }

    struct ActorFixture {
        deps: ServerDeps,
        agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
        _tmp: tempfile::TempDir,
    }

    async fn actor_fixture() -> ActorFixture {
        let tmp = tempfile::tempdir().unwrap();
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        let mut vendors = HashMap::new();
        vendors.insert("mock".to_string(), agent.link());
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        ActorFixture {
            deps,
            agent,
            _tmp: tmp,
        }
    }

    /// A supervisor stand-in for tests that spawn a bare `SessionActor`: it
    /// answers nothing, and exists only so `report()`'s `.tell()` has a live
    /// mailbox to land in.
    struct DeafSupervisor;

    #[async_trait]
    impl EventSourcedActor for DeafSupervisor {
        type Command = SessionSupervisorCommand;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("test", "deaf-supervisor")
        }
        fn initial_state() {}
        fn apply_event((): (), (): ()) {}
        async fn handle_command(
            &mut self,
            (): &(),
            _cmd: SessionSupervisorCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            CommandEffect::none()
        }
    }

    /// The frame channel a supervisor would hand the actor. Owned by the test,
    /// exactly as the real one is owned by the supervisor rather than the actor.
    fn test_frames() -> broadcast::Sender<SessionFrame> {
        broadcast::channel(FRAME_BROADCAST_CAPACITY).0
    }

    fn spawn_deaf_supervisor() -> ActorRef<SessionSupervisorCommand> {
        horsie_actor::spawn_root(
            DeafSupervisor,
            Arc::new(horsie_actor::InMemoryJournal::new()),
        )
    }

    #[tokio::test]
    async fn drain_does_nothing_when_the_inbox_is_empty() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let actions = decisions(&actor, &SessionState::default());
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn drain_does_nothing_while_a_turn_is_already_running() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ]);
        let actions = decisions(&actor, &state);
        assert!(
            actions.is_empty(),
            "a run in flight must never be drained into a second one"
        );
    }

    #[tokio::test]
    async fn drain_refuses_once_the_session_is_unrecoverable() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::SessionFailed {
                at_ms: 0,
                reason: "runtime gone".into(),
            },
        ]);
        let actions = decisions(&actor, &state);
        assert!(
            actions.is_empty(),
            "a terminal session must never start another turn"
        );
    }

    /// A failed turn is a turn boundary that deliberately does *not* drain. The
    /// cause is usually stuck — an expired key, a dead vendor — and draining
    /// would turn three queued messages into three back-to-back failures the
    /// user never asked for. The next message they send drains them.
    #[tokio::test]
    async fn a_failed_turn_does_not_drain() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let id = Uuid::new_v4();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        // A turn is running, and a message arrived while it was.
        let prior = [
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ];
        let bytes: Vec<Vec<u8>> = prior
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&SessionActor::persistence_id_for(id), &bytes)
            .await
            .unwrap();
        let session = horsie_actor::spawn_root(
            SessionActor::new(id, actor_spec_fixture(), f.deps, parent, test_frames()),
            journal.clone(),
        );
        // Recovery reconciles the interrupted turn first (event 4); wait for
        // that to settle so the failure is the only thing left to observe.
        wait_for_journal_len(&journal, id, 4).await;

        session
            .tell(SessionCommand::AgentOutcome(AgentOutcome::Failed {
                session_id: id,
                error: "provider exploded".into(),
                recoverable: true,
                terminal: false,
            }))
            .await
            .unwrap();

        // The failure lands (event 5) — and nothing follows: no drain into a
        // back-to-back failure.
        wait_for_journal_len(&journal, id, 5).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            session_journal_len(&journal, id).await,
            5,
            "a failed turn records the failure and nothing else"
        );
        // Asked of the actor, which is the only thing that reads this journal.
        let snapshot = session
            .ask(|reply| SessionCommand::Snapshot { reply })
            .await
            .unwrap();
        assert!(matches!(
            snapshot.status,
            crate::sessions::spec::SessionStatus::Failed { .. }
        ));
        assert_eq!(snapshot.inbox.len(), 1, "the queued message is still owed");
    }

    /// Stop is a turn boundary like any other: it cancels the turn, not the
    /// promise. Whatever was queued while the cancelled turn ran starts the
    /// next one immediately — which is exactly why the client marks queued
    /// messages as unread, so that next turn does not look self-inflicted.
    #[tokio::test]
    async fn stop_then_a_queued_message_starts_the_next_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let running = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ]);

        let stopped =
            SessionActor::apply_event(running, SessionDomainEvent::TurnStopped { at_ms: 0 });
        assert_eq!(stopped.status, SessionStatus::Idle);
        let actions = decisions(&actor, &stopped);
        assert_eq!(actions.len(), 1, "{actions:?}");
        let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m2".to_string()]);
    }

    #[tokio::test]
    async fn drain_consumes_the_whole_inbox_and_starts_a_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let state = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let actions = decisions(&actor, &state);
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m1".to_string(), "m2".to_string()]);
    }

    #[tokio::test]
    async fn drain_abandons_pending_asks_rather_than_answering_them() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let state = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which?".into(),
            },
            queued("m1", "main"),
        ]);
        let actions = decisions(&actor, &state);
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn {
            consumed,
            answered,
            input,
            ..
        } = &actions[0]
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m1".to_string()]);
        assert_eq!(
            input.results.len(),
            1,
            "the parked call still gets a result"
        );
        assert!(input.results[0].is_error);
        assert!(
            answered.is_empty(),
            "a plain message abandons the question rather than answering it — \
             answers come through `Answer`, which requires all of them at once"
        );
    }

    /// A session parked on two questions, with an actor to answer them on.
    async fn parked_on_two_asks() -> (SessionActor, SessionState) {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let state = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-2".into()),
                question: "which model?".into(),
            },
        ]);
        (actor, state)
    }

    fn answer(id: &str, text: &str) -> AskAnswer {
        AskAnswer {
            tool_call_id: id.to_string(),
            text: text.to_string(),
        }
    }

    #[tokio::test]
    async fn a_partial_answer_set_is_refused_and_journals_nothing() {
        // Resuming on half the answers would send the provider a `tool_use` with
        // no result, which is exactly the 400 this whole change exists to stop.
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(&state, vec![answer("call-1", "main")], tx)
            .await;

        assert!(
            effect.events().is_empty(),
            "a refused answer set changes nothing"
        );
        match rx.await.unwrap() {
            Err(AnswerError::Incomplete {
                missing,
                unexpected,
            }) => {
                assert_eq!(missing, vec!["call-2".to_string()]);
                assert!(unexpected.is_empty());
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_answer_for_a_call_that_is_not_pending_is_refused() {
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(
                &state,
                vec![
                    answer("call-1", "main"),
                    answer("call-2", "kimi"),
                    answer("call-9", "who asked?"),
                ],
                tx,
            )
            .await;

        assert!(effect.events().is_empty());
        match rx.await.unwrap() {
            Err(AnswerError::Incomplete { unexpected, .. }) => {
                assert_eq!(unexpected, vec!["call-9".to_string()]);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_complete_answer_set_begins_a_turn_naming_every_ask() {
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(
                &state,
                vec![answer("call-1", "main"), answer("call-2", "kimi")],
                tx,
            )
            .await;

        assert!(rx.await.unwrap().is_ok());
        let events = effect.events();
        assert_eq!(events.len(), 1);
        let SessionDomainEvent::TurnBegan {
            consumed, answered, ..
        } = &events[0]
        else {
            panic!("expected TurnBegan, got {:?}", events[0]);
        };
        assert!(consumed.is_empty(), "an answer consumes no queued message");
        let mut answered = answered.clone();
        answered.sort();
        assert_eq!(answered, vec!["call-1".to_string(), "call-2".to_string()]);

        // And the park is over: folding the event clears every pending ask.
        let next = SessionActor::apply_event(state, events[0].clone());
        assert!(next.pending_asks.is_empty());
        assert_eq!(next.status, SessionStatus::Running);
    }

    #[tokio::test]
    async fn answering_a_session_that_is_not_parked_is_refused() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            test_frames(),
        );
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(&SessionState::default(), vec![answer("call-1", "main")], tx)
            .await;

        assert!(effect.events().is_empty());
        assert_eq!(rx.await.unwrap(), Err(AnswerError::NothingPending));
    }

    /// An `LlmProvider` that hangs until released, so a test can hold a run
    /// genuinely `Running` for as long as it needs to.
    struct BlockingProvider {
        gate: tokio::sync::Notify,
    }

    impl BlockingProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                gate: tokio::sync::Notify::new(),
            })
        }
        fn release(&self) {
            self.gate.notify_one();
        }
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            self.gate.notified().await;
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "done".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    #[tokio::test]
    async fn prepare_offload_refuses_while_a_run_is_in_flight() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let provider = BlockingProvider::new();
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider.clone() as Arc<dyn LlmProvider>);

        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                parent,
                test_frames(),
            ),
            journal,
        );

        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "go".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();

        let offloadable = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .unwrap();
        assert!(
            !offloadable,
            "a run in flight must never be offloaded out from under itself"
        );
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime: {:?}",
            f.agent.signals()
        );

        // Refusing must leave the actor exactly as it was, still answering
        // commands normally rather than having torn itself down.
        provider.release();
        let (tx, rx) = oneshot::channel();
        session
            .tell(SessionCommand::UsageStats { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap();
    }

    /// A provider whose every call immediately ends the turn with plain text.
    struct EchoProvider;

    #[async_trait]
    impl LlmProvider for EchoProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "sub answer".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    async fn spawn_session_with_provider(
        provider: Arc<dyn LlmProvider>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Uuid,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider);
        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                parent,
                test_frames(),
            ),
            journal.clone(),
        );
        (f, session, id, journal)
    }

    /// A two-step run: `triage` branches on its output to `fix` or `file`.
    fn run_spec_fixture(input: &str) -> crate::sessions::workflow::WorkflowRunSpec {
        use crate::sessions::workflow::{TransitionSpec, WorkflowRunSpec, WorkflowStepSpec};
        let settings = |()| AgentSettings {
            model: "mock".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
        };
        WorkflowRunSpec {
            workflow: "fix-bug".into(),
            start: "triage".into(),
            steps: vec![
                WorkflowStepSpec {
                    name: "triage".into(),
                    agent: "triager".into(),
                    prompt: "Triage it.".into(),
                    output_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"severity": {"type": "string"}}
                    })),
                    transitions: vec![
                        TransitionSpec {
                            to: "fix".into(),
                            condition: Some("output.severity == \"p0\"".into()),
                        },
                        TransitionSpec {
                            to: "file".into(),
                            condition: None,
                        },
                    ],
                    settings: settings(()),
                },
                WorkflowStepSpec {
                    name: "fix".into(),
                    agent: "coder".into(),
                    prompt: "Fix it.".into(),
                    output_schema: None,
                    transitions: vec![],
                    settings: settings(()),
                },
                WorkflowStepSpec {
                    name: "file".into(),
                    agent: "writer".into(),
                    prompt: "File it.".into(),
                    output_schema: None,
                    transitions: vec![],
                    settings: settings(()),
                },
            ],
            input: input.to_string(),
            max_steps: 100,
        }
    }

    /// A session that is a run of [`run_spec_fixture`], on a scripted provider.
    async fn spawn_run_with_provider(
        provider: Arc<dyn LlmProvider>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Uuid,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let mut spec = actor_spec_fixture();
        spec.origin = crate::sessions::spec::SessionOrigin::Workflow {
            workflow: "fix-bug".into(),
        };
        spec.workflow = Some(Arc::new(run_spec_fixture("the build is red")));
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &spec)
            .await
            .expect("create");
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider);
        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(id, spec, f.deps.clone(), parent, test_frames()),
            journal.clone(),
        );
        (f, session, id, journal)
    }

    /// Poll the folded run until `pred` holds (2s cap).
    async fn wait_for_run(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
        pred: impl Fn(&crate::sessions::workflow::WorkflowRunState) -> bool,
    ) -> crate::sessions::workflow::WorkflowRunState {
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(journal, session_id).await;
            if let Some(run) = state.mode.run()
                && pred(run)
            {
                return run.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        panic!("run never satisfied the predicate: {:?}", state.mode.run());
    }

    /// A scripted `conclude` call carrying this output.
    ///
    /// A step that has an output schema *and* may ask gets the kind-tagged
    /// conclude schema, so the output nests under `output` rather than being
    /// the payload — sending it bare submits `null`, and every condition reads
    /// false.
    fn concludes(output: serde_json::Value) -> horsie_agentcore::CompletionResponse {
        horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::ToolCall(
                horsie_agentcore::ToolCallPart {
                    id: "c-1".into(),
                    name: "conclude".into(),
                    input: serde_json::json!({"kind": "submit", "output": output}),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::ToolUse,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        }
    }

    /// The whole point: a run starts itself, its first step's output picks the
    /// branch, and the branch's step ends the run.
    #[tokio::test]
    async fn a_run_starts_itself_and_routes_on_its_first_steps_output() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "fixed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
            ),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;

        // Nobody sent a message: creating the run is what starts it.
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;

        let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(
            visited,
            vec!["triage", "fix"],
            "p0 must route to `fix`; triage concluded with {:?}",
            run.steps[0].output
        );
        // The condition that matched is recorded, which is what draws the edge.
        assert_eq!(
            run.steps[1].via.as_deref(),
            Some("output.severity == \"p0\"")
        );
        assert_eq!(run.steps[1].from, Some(0));
        // Each step is its own agent, derived from the session and the index.
        assert_eq!(
            run.steps[0].agent,
            crate::sessions::workflow::WorkflowRunSpec::step_agent_id(id, 0)
        );
        assert_ne!(run.steps[0].agent, run.steps[1].agent);
        // The second step was handed the first's output under a header.
        assert!(
            run.steps[1].input.contains("## Input from step `triage`"),
            "{}",
            run.steps[1].input
        );
        assert!(run.steps[1].input.starts_with("Fix it."));
    }

    /// The `else` branch, and the run's output being the last step's.
    #[tokio::test]
    async fn a_non_matching_condition_takes_the_catch_all() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"severity": "p2"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "filed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
            ),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(visited, vec!["triage", "file"]);
        assert!(run.steps[1].via.is_none());
    }

    /// A run works from its definition; there is nobody to send a message to.
    #[tokio::test]
    async fn a_run_refuses_a_user_message() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(Script::of([Ok(concludes(
            serde_json::json!({"severity": "p0"}),
        ))]));
        let (_f, session, _id, _journal) = spawn_run_with_provider(provider).await;
        let err = session
            .ask(|reply| SessionCommand::UserMessage {
                text: "hello".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::Rejected(_)), "{err:?}");
    }

    /// Retrying appends an attempt rather than replacing one, so the earlier
    /// attempt stays readable and the graph can stack them.
    #[tokio::test]
    async fn retrying_a_step_appends_an_attempt_on_the_same_edge() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "fixed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
            ),
        );
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
        wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;

        session
            .ask(|reply| SessionCommand::RetryStep { index: 1, reply })
            .await
            .unwrap()
            .unwrap();
        let run = wait_for_run(&journal, id, |r| r.steps.len() == 3).await;
        assert_eq!(run.steps[2].step, "fix");
        assert_eq!(run.steps[2].attempt, 2, "the retry numbers itself");
        // It sits where the original sat, so it draws on the same edge.
        assert_eq!(run.steps[2].from, run.steps[1].from);
        assert_eq!(run.steps[2].via, run.steps[1].via);
        // The first attempt is untouched.
        assert_eq!(
            run.steps[1].status,
            crate::sessions::workflow::StepStatus::Concluded
        );
    }

    /// Poll the session's folded state until the tree satisfies `pred` (2s
    /// cap). Subagent progress is journal-first, so the fold is the honest
    /// thing to wait on.
    async fn wait_for_tree(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
        pred: impl Fn(&crate::sessions::subagents::SubAgentTree) -> bool,
    ) {
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(journal, session_id).await;
            if pred(state.mode.subagents()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("tree condition not met within 2s");
    }

    /// Entry count of the session's own journal (`session/<id>`), not the
    /// agent's.
    async fn session_journal_len(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
    ) -> u64 {
        use futures_util::StreamExt;
        let pid = SessionActor::persistence_id_for(session_id);
        let mut count = 0u64;
        #[expect(
            clippy::disallowed_methods,
            reason = "test-only inspection: counts what was journaled, which no actor reports"
        )]
        let mut stream = journal.replay(&pid, 0).await;
        while let Some(item) = stream.next().await {
            if item.is_ok() {
                count += 1;
            }
        }
        count
    }

    /// Poll the session's own journal until it holds at least `n` entries
    /// (2s cap).
    async fn wait_for_journal_len(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
        n: u64,
    ) {
        for _ in 0..200 {
            if session_journal_len(journal, session_id).await >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("journal did not reach {n} entries within 2s");
    }

    /// Wraps a journal and counts `replay` calls, so a test can assert that
    /// serving reads and streams never touches durable storage.
    struct CountingJournal {
        inner: horsie_actor::InMemoryJournal,
        replays: std::sync::atomic::AtomicUsize,
    }

    impl CountingJournal {
        fn new() -> Self {
            Self {
                inner: horsie_actor::InMemoryJournal::new(),
                replays: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn replays(&self) -> usize {
            self.replays.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl horsie_actor::Journal for CountingJournal {
        async fn persist(
            &self,
            pid: &horsie_actor::PersistenceId,
            events: &[Vec<u8>],
        ) -> horsie_actor::JournalResult<()> {
            self.inner.persist(pid, events).await
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "this decorator's whole job is to count the inner journal's replays"
        )]
        async fn replay(
            &self,
            pid: &horsie_actor::PersistenceId,
            after_seq: u64,
        ) -> futures_util::stream::BoxStream<'_, horsie_actor::JournalResult<(u64, Vec<u8>)>>
        {
            self.replays
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.replay(pid, after_seq).await
        }

        async fn save_snapshot(
            &self,
            pid: &horsie_actor::PersistenceId,
            state: Vec<u8>,
            seq_nr: u64,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.save_snapshot(pid, state, seq_nr).await
        }

        async fn latest_snapshot(
            &self,
            pid: &horsie_actor::PersistenceId,
        ) -> horsie_actor::JournalResult<Option<(Vec<u8>, u64)>> {
            self.inner.latest_snapshot(pid).await
        }

        async fn delete_events_before(
            &self,
            pid: &horsie_actor::PersistenceId,
            seq_nr: u64,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.delete_events_before(pid, seq_nr).await
        }

        async fn copy_snapshot(
            &self,
            from: &horsie_actor::PersistenceId,
            to: &horsie_actor::PersistenceId,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.copy_snapshot(from, to).await
        }

        async fn clear(
            &self,
            pid: &horsie_actor::PersistenceId,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.clear(pid).await
        }
    }

    /// Drain whatever the agent stream has produced so far.
    fn drain_frames(rx: &mut broadcast::Receiver<AgentFrame>) -> Vec<AgentFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
        }
        out
    }

    fn appended_ids(frames: &[AgentFrame]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| match f {
                AgentFrame::Appended { entry } => Some(entry.id().to_string()),
                AgentFrame::Delta { .. }
                | AgentFrame::ToolStart { .. }
                | AgentFrame::TurnCompleted { .. }
                | AgentFrame::TaskListChanged { .. } => None,
            })
            .collect()
    }

    /// The invariant the old two-vocabulary design could not even state: the
    /// transcript you get by accumulating stream appends is the transcript
    /// `/history` hands you. They are two projections of one append-only log.
    #[tokio::test]
    async fn the_stream_and_history_agree_on_the_transcript() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let mut rx = session
            .ask(|reply| SessionCommand::SubscribeAgent {
                agent_id: None,
                reply,
            })
            .await
            .unwrap()
            .expect("the main agent is subscribable");

        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "go".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        // Let the turn's appends land on the broadcast.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let streamed = appended_ids(&drain_frames(&mut rx));
        let stored: Vec<String> = main_history(&session)
            .await
            .entries
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert!(!streamed.is_empty(), "the turn must produce appends");
        assert_eq!(
            streamed, stored,
            "stream appends and history must be the same transcript, in the same order"
        );
    }

    /// Reads and streams are served from actor state. The journal is touched
    /// only while an actor recovers — never to answer a query.
    #[tokio::test]
    async fn serving_reads_never_touches_the_journal() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
        );
        let counting = Arc::new(CountingJournal::new());
        let journal: Arc<dyn horsie_actor::Journal> = counting.clone();
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                test_frames(),
            ),
            journal.clone(),
        );

        // Drive one turn so both actors are loaded and have history.
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "go".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Recovery is allowed to replay; everything after it is not.
        let after_recovery = counting.replays();
        assert!(
            after_recovery > 0,
            "the counter must actually observe recovery, or this test proves nothing"
        );

        let _ = main_history(&session).await;
        let _ = session
            .ask(|reply| SessionCommand::History {
                agent_id: None,
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    after: Some("m1".into()),
                    limit: 10,
                },
                reply,
            })
            .await
            .unwrap();
        let _ = session
            .ask(|reply| SessionCommand::AgentState {
                agent_id: None,
                reply,
            })
            .await
            .unwrap();
        let _ = session
            .ask(|reply| SessionCommand::SubscribeAgent {
                agent_id: None,
                reply,
            })
            .await
            .unwrap();

        assert_eq!(
            counting.replays(),
            after_recovery,
            "history, agent state and subscribe must all be served from memory"
        );
    }

    async fn spawn_sub(session: &ActorRef<SessionCommand>, label: &str, task: &str) -> Uuid {
        session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::Main,
                label: label.into(),
                task: task.into(),
                agent_type: None,
                reply,
            })
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn spawn_records_a_running_subagent_in_the_tree() {
        // Completion routing lands with outcome handling (next task); here the
        // spawn itself is what must be durable and attributed.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = state.mode.subagents().get(&sub).unwrap();
        assert_eq!(rec.depth, 1);
        assert_eq!(rec.parent, crate::sessions::subagents::SubAgentParent::Main);
        assert_eq!(rec.label, "research");
        assert_eq!(rec.task, "dig into it");
    }

    #[tokio::test]
    async fn spawn_beyond_depth_four_is_rejected() {
        // A hanging provider keeps every spawned node Running, so the chain
        // builds deterministically: Main → d1 → d2 → d3 → d4, and d4's spawn
        // is refused.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let mut parent = crate::sessions::subagents::SubAgentParent::Main;
        for _ in 0..4 {
            let id_child = session
                .ask(|reply| SessionCommand::SpawnSubAgent {
                    caller: parent,
                    label: "w".into(),
                    task: "t".into(),
                    agent_type: None,
                    reply,
                })
                .await
                .unwrap()
                .unwrap();
            wait_for_tree(&journal, id, |t| t.has_active()).await;
            parent = crate::sessions::subagents::SubAgentParent::SubAgent(id_child);
        }
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: parent,
                label: "x".into(),
                task: "y".into(),
                agent_type: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "max subagent depth 4 reached");
    }

    #[tokio::test]
    async fn spawn_beyond_the_concurrency_cap_is_rejected() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        for _ in 0..8 {
            let _ = spawn_sub(&session, "w", "t").await;
        }
        wait_for_tree(&journal, id, |t| t.active_count() == 8).await;
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::Main,
                label: "x".into(),
                task: "y".into(),
                agent_type: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "8 subagents already active");
    }

    #[tokio::test]
    async fn spawn_from_an_unknown_caller_is_rejected() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::SubAgent(Uuid::new_v4()),
                label: "x".into(),
                task: "y".into(),
                agent_type: None,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "caller is not a known agent");
    }

    #[tokio::test]
    async fn subagent_toolbox_strips_session_metadata_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

        let build = |kind: SessionAgentKind| SessionContextProvider {
            agent_type: None,
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind,
            unattended: false,
            session: session.clone(),
            frames: broadcast::channel(8).0,
            last_client: Mutex::new(None),
        };

        let main = build(SessionAgentKind::Main).provide().await.unwrap();
        let main_tools: Vec<String> = main.toolbox.specs().into_iter().map(|s| s.name).collect();
        for t in [
            "spawn_agent",
            "subagent_status",
            "set_session_title",
            "ask_user",
        ] {
            assert!(main_tools.contains(&t.to_string()), "main lacks {t}");
        }

        let sub_id = Uuid::new_v4();
        let sub = build(SessionAgentKind::Sub(sub_id))
            .provide()
            .await
            .unwrap();
        let sub_tools: Vec<String> = sub.toolbox.specs().into_iter().map(|s| s.name).collect();
        for t in ["spawn_agent", "subagent_status"] {
            assert!(sub_tools.contains(&t.to_string()), "sub lacks {t}");
        }
        for t in ["set_session_title", "ask_user"] {
            assert!(!sub_tools.contains(&t.to_string()), "sub must not have {t}");
        }
        assert!(
            sub.system_prompt.unwrap().contains("# Subagent role"),
            "the subagent prompt must explain its role"
        );
    }

    #[tokio::test]
    async fn a_zero_subagent_cap_hides_the_spawn_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let mut settings = actor_spec_fixture().agent;
        settings.max_concurrent_subagents = Some(0);
        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended: false,
            session: session.clone(),
            frames: broadcast::channel(8).0,
            last_client: Mutex::new(None),
        };
        let tools: Vec<String> = provider
            .provide()
            .await
            .unwrap()
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        // Disabled, not merely unusable: an advertised tool that always
        // rejects reads as a bug to the model.
        for t in ["spawn_agent", "subagent_status"] {
            assert!(!tools.contains(&t.to_string()), "disabled session has {t}");
        }
    }

    #[tokio::test]
    async fn an_unattended_session_is_offered_no_ask_user_tool() {
        // A routine run has nobody to answer a question: offering `ask_user`
        // would let the agent park the run forever. The prompt has to say so
        // too -- the base prompt tells the model the tool exists.
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let build = |unattended: bool| SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended,
            session: session.clone(),
            frames: broadcast::channel(8).0,
            last_client: Mutex::new(None),
        };
        let names = |c: &Contexts| -> Vec<String> {
            c.toolbox.specs().into_iter().map(|s| s.name).collect()
        };

        let unattended = build(true).provide().await.unwrap();
        let tools = names(&unattended);
        assert!(!tools.contains(&ASK_USER_TOOL.to_string()));
        // Everything else the main agent has is untouched.
        assert!(tools.contains(&"set_session_title".to_string()));
        assert!(tools.contains(&"spawn_agent".to_string()));
        assert!(
            unattended
                .system_prompt
                .unwrap()
                .contains("# Unattended run"),
            "an unattended run must be told there is no user"
        );

        let attended = build(false).provide().await.unwrap();
        assert!(names(&attended).contains(&ASK_USER_TOOL.to_string()));
        assert!(!attended.system_prompt.unwrap().contains("# Unattended run"));
    }

    #[test]
    fn a_subagent_gets_its_own_runtime_identity() {
        let client = horsie_runtime_client::RuntimeClient::new(
            horsie_runtime_client::MockTransport::ok(""),
            "session-id",
        );
        let main = scoped_client(&SessionAgentKind::Main, client.clone());
        assert_eq!(main.agent_id(), "session-id");

        let sub_id = Uuid::new_v4();
        let sub = scoped_client(&SessionAgentKind::Sub(sub_id), client);
        assert_eq!(sub.agent_id(), sub_id.to_string());
    }

    fn user_texts(page: &horsie_workflow::AgentHistoryPage) -> Vec<String> {
        page.messages()
            .filter(|m| m.role == horsie_agentcore::Role::User)
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                horsie_agentcore::ContentPart::Text(t) => Some(t.text.clone()),
                horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::ToolResult(_)
                | horsie_agentcore::ContentPart::Thinking(_)
                | horsie_agentcore::ContentPart::SubAgentResult(_) => None,
            })
            .collect()
    }

    /// A user message's subagent-result parts, rendered the way the wire sees
    /// them — the counterpart to `user_texts` now that a result is a part of
    /// its own rather than text merged into what the person said.
    fn subagent_texts(page: &horsie_workflow::AgentHistoryPage) -> Vec<String> {
        page.messages()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                horsie_agentcore::ContentPart::SubAgentResult(r) => Some(r.to_wire_text()),
                horsie_agentcore::ContentPart::Text(_)
                | horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::ToolResult(_)
                | horsie_agentcore::ContentPart::Thinking(_) => None,
            })
            .collect()
    }

    fn hook_record(plugin: &str, call: &str) -> HookRecord {
        HookRecord {
            plugin: plugin.to_string(),
            duration_ms: 4,
            halt: None,
            action: horsie_models::hooks::HookAction::PreToolUse(
                horsie_models::hooks::PreToolUseRecord {
                    call: horsie_models::hooks::ToolScope {
                        tool: "bash".to_string(),
                        tool_call_id: call.to_string(),
                    },
                    system_message: None,
                    outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                        horsie_models::hooks::HookDenied {
                            reason: Some("not allowed".into()),
                        },
                    ),
                },
            ),
        }
    }

    async fn agent_history(
        session: &ActorRef<SessionCommand>,
        agent_id: Option<String>,
    ) -> horsie_workflow::AgentHistoryPage {
        session
            .ask(|reply| SessionCommand::History {
                agent_id,
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    after: None,
                    limit: 50,
                },
                reply,
            })
            .await
            .unwrap()
            .expect("agent history")
    }

    fn hook_ids(page: &horsie_workflow::AgentHistoryPage) -> Vec<String> {
        page.entries
            .iter()
            .filter_map(|e| match e {
                horsie_agentcore::HistoryEntry::Hook(h) => Some(h.id.clone()),
                horsie_agentcore::HistoryEntry::Llm(_) => None,
            })
            .collect()
    }

    // --- `Stop` continuation ---
    //
    // `Stop` is the only event whose two capabilities are both ways of *not*
    // ending a turn, so these assert on what happens to the turn rather than on
    // what was recorded. `FakeRuntimeVendor` answers the protocol itself, so
    // they script records; real command execution is covered one layer down, in
    // `runtime/src/hooks/server.rs`.

    fn stop_record(outcome: StopOutcome) -> HookRecord {
        HookRecord {
            plugin: "stopper".into(),
            duration_ms: 1,
            halt: None,
            action: HookAction::Stop(horsie_models::hooks::StopRecord {
                system_message: None,
                outcome,
            }),
        }
    }

    fn stop_blocked(reason: &str) -> Vec<HookRecord> {
        vec![stop_record(StopOutcome::Blocked(
            horsie_models::hooks::HookBlocked {
                reason: Some(reason.to_string()),
            },
        ))]
    }

    /// An `EchoProvider` that also keeps every text part it was prompted with,
    /// so a test can assert on what the model was actually told rather than on
    /// what the transcript would translate to.
    #[derive(Default)]
    struct PromptRecorder(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl LlmProvider for PromptRecorder {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            for m in request.messages {
                for p in &m.parts {
                    if let horsie_agentcore::ContentPart::Text(t) = p {
                        seen.push(t.text.clone());
                    }
                }
            }
            drop(seen);
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "done".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    /// A session whose runtime answers every `RunHooks` from `records`, with an
    /// LLM that concludes on the first call.
    async fn stop_harness(
        records: Vec<Vec<HookRecord>>,
    ) -> (ActorFixture, ActorRef<SessionCommand>) {
        let (f, session, _, _, _) = stop_harness_full(records).await;
        (f, session)
    }

    /// The same harness, also handing back every prompt the model was sent.
    async fn stop_harness_with_prompts(
        records: Vec<Vec<HookRecord>>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let (f, session, prompts, _, _) = stop_harness_full(records).await;
        (f, session, prompts)
    }

    /// The same harness, also handing back the journal, for a test that has to
    /// read what was *persisted*. A spurious failure is overwritten in the
    /// status by whatever lands next; the journal keeps it.
    async fn stop_harness_with_journal(
        records: Vec<Vec<HookRecord>>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Uuid,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let (f, session, _, id, journal) = stop_harness_full(records).await;
        (f, session, id, journal)
    }

    async fn stop_harness_full(
        records: Vec<Vec<HookRecord>>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Arc<Mutex<Vec<String>>>,
        Uuid,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
            .hook_records(records)
            .serve_in_process()
            .await
            .expect("fake agent");
        let mut vendors = HashMap::new();
        vendors.insert("mock".to_string(), agent.link());
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let f = ActorFixture {
            deps,
            agent,
            _tmp: tmp,
        };
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>,
        );
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                test_frames(),
            ),
            journal.clone(),
        );
        (f, session, prompts, id, journal)
    }

    /// Every user-role message in the main agent's transcript, in order — which
    /// is one per turn, so its length is the number of turns that ran.
    async fn turn_inputs(session: &ActorRef<SessionCommand>) -> Vec<String> {
        agent_history(session, None)
            .await
            .entries
            .iter()
            .filter_map(|e| match e {
                horsie_agentcore::HistoryEntry::Llm(m)
                    if m.role == horsie_agentcore::Role::User =>
                {
                    Some(m.parts.iter().fold(String::new(), |mut acc, p| {
                        if let horsie_agentcore::ContentPart::Text(t) = p {
                            acc.push_str(&t.text);
                        }
                        acc
                    }))
                }
                horsie_agentcore::HistoryEntry::Llm(_)
                | horsie_agentcore::HistoryEntry::Hook(_) => None,
            })
            .collect()
    }

    /// The `Stop` outcomes journaled on the main agent's transcript.
    async fn stop_outcomes(session: &ActorRef<SessionCommand>) -> Vec<StopOutcome> {
        agent_history(session, None)
            .await
            .entries
            .iter()
            .filter_map(|e| match e {
                horsie_agentcore::HistoryEntry::Hook(h) => match &h.record.action {
                    HookAction::Stop(r) => Some(r.outcome.clone()),
                    other => panic!("only Stop hooks run in these tests, got {other:?}"),
                },
                horsie_agentcore::HistoryEntry::Llm(_) => None,
            })
            .collect()
    }

    /// Wait until the transcript stops growing, so a test asserting "no further
    /// turn ran" observes a real stop rather than a race it won.
    async fn settled_inputs(session: &ActorRef<SessionCommand>) -> Vec<String> {
        let mut last = turn_inputs(session).await;
        let mut stable = 0;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let now = turn_inputs(session).await;
            if now == last {
                stable += 1;
                if stable == 5 {
                    return now;
                }
            } else {
                stable = 0;
                last = now;
            }
        }
        last
    }

    async fn send(session: &ActorRef<SessionCommand>, text: &str) {
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: text.into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
    }

    /// A blocking `Stop` means *blocked from stopping*: the turn does not
    /// conclude, and the reason becomes the input to another run. The opposite
    /// of a `PreToolUse` refusal.
    #[tokio::test]
    async fn a_blocking_stop_hook_starts_another_run_with_its_reason() {
        let (_f, session) = stop_harness(vec![
            stop_blocked("tests still failing"),
            vec![stop_record(StopOutcome::Ran(
                horsie_models::hooks::ContextInjected {
                    additional_context: None,
                },
            ))],
        ])
        .await;
        send(&session, "do the thing").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(inputs.len(), 2, "the turn continued once: {inputs:?}");
        assert!(inputs[0].contains("do the thing"), "{inputs:?}");
        assert!(inputs[1].contains("tests still failing"), "{inputs:?}");
    }

    /// The loop guard that must not be optional: horsie runs unattended
    /// sessions, so a hook ignoring `stop_hook_active` would spin forever with
    /// nobody watching.
    #[tokio::test]
    async fn an_unconditionally_blocking_stop_hook_is_stopped_by_the_cap() {
        let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(
            inputs.len(),
            1 + MAX_STOP_CONTINUATIONS,
            "the original turn plus exactly the cap: {inputs:?}"
        );
    }

    /// And the record says the cap ended it, rather than looking like a turn
    /// that ended on its own.
    #[tokio::test]
    async fn the_capped_continuation_is_recorded_as_cap_reached() {
        let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
        send(&session, "go").await;
        settled_inputs(&session).await;
        let outcomes = stop_outcomes(&session).await;
        assert!(
            matches!(outcomes.last(), Some(StopOutcome::CapReached(_))),
            "the last record must name the cap, got {outcomes:?}"
        );
    }

    /// Non-blocking feedback informs the model; it does not force a turn.
    /// Starting a run on it would make every advisory `Stop` hook an infinite
    /// session.
    #[tokio::test]
    async fn non_blocking_additional_context_does_not_start_a_run() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
            horsie_models::hooks::ContextInjected {
                additional_context: Some("consider the tests".into()),
            },
        ))]])
        .await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(inputs.len(), 1, "informed, not forced: {inputs:?}");
    }

    /// `continue: false` outranks `decision: "block"`, which is the spec's own
    /// precedence — and the one seam where that precedence is observable. The
    /// same record blocks *and* halts; the turn ends rather than continuing.
    #[tokio::test]
    async fn a_halt_beats_a_blocking_stop_hook() {
        let mut blocking = stop_blocked("tests still failing");
        blocking[0].halt = Some(horsie_models::hooks::HookHalt {
            reason: Some("out of budget".into()),
        });
        let (_f, session, id, journal) = stop_harness_with_journal(vec![blocking]).await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(
            inputs.len(),
            1,
            "the halt must stop the block continuing the turn: {inputs:?}"
        );
        // And ends it *cleanly*. `run_hooks` puts its records on the same sink
        // tool records take, so before `tool_halt_reason` narrowed what the sink
        // acts on, this halt also arrived as a `HaltAgent` and failed the turn
        // the stop seam had already concluded. Read off the journal rather than
        // the status: `TurnEnded` lands after the spurious failure and hides it.
        let events = journaled_events(&journal, id).await;
        assert!(
            !events.iter().any(|e| e.contains("TurnFailed")),
            "a halted stop ends the turn, it does not fail it: {events:?}"
        );
    }

    /// Every session event that reached the journal, as its serialized payload.
    /// Matched on as text, because the variant name is what a test cares about
    /// and decoding buys nothing over reading it.
    async fn journaled_events(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
    ) -> Vec<String> {
        use futures_util::StreamExt;
        let pid = SessionActor::persistence_id_for(session_id);
        #[expect(
            clippy::disallowed_methods,
            reason = "test-only inspection: names what was journaled, which no actor reports"
        )]
        let mut stream = journal.replay(&pid, 0).await;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok((_, bytes)) = item {
                out.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        out
    }

    // --- Plugin agents ---

    /// A session whose runtime library declares `code-reviewer`, with a
    /// `PromptRecorder` so the test can assert what the model was actually
    /// told rather than what the transcript would render.
    async fn agent_harness() -> (ActorFixture, ActorRef<SessionCommand>, Uuid) {
        let tmp = tempfile::tempdir().unwrap();
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
            .shared_agents(vec![horsie_models::runtime::PluginAgent {
                plugin: "feature-dev".into(),
                rel_path: "feature-dev/agents/code-reviewer.md".into(),
                content: "---\nname: code-reviewer\ndescription: reviews diffs\n\
                          tools: Read, Grep\n---\nReport only high-confidence bugs."
                    .into(),
            }])
            .serve_in_process()
            .await
            .expect("fake agent");
        let mut vendors = HashMap::new();
        vendors.insert("mock".to_string(), agent.link());
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let f = ActorFixture {
            deps,
            agent,
            _tmp: tmp,
        };
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>,
        );
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                test_frames(),
            ),
            Arc::new(horsie_actor::InMemoryJournal::new()) as Arc<dyn horsie_actor::Journal>,
        );
        drop(prompts);
        (f, session, id)
    }

    async fn spawn_typed(
        session: &ActorRef<SessionCommand>,
        agent_type: Option<&str>,
    ) -> Result<Uuid, String> {
        session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::Main,
                label: "review".into(),
                task: "look at the diff".into(),
                agent_type: agent_type.map(str::to_string),
                reply,
            })
            .await
            .unwrap()
    }

    /// The agent's body replaces the generic subagent role, and its `tools`
    /// allowlist reaches the toolbox through the same alias table hook matchers
    /// use.
    #[tokio::test]
    async fn a_typed_subagent_runs_with_its_plugins_prompt() {
        let (f, session, id) = agent_harness().await;
        let sub = spawn_typed(&session, Some("code-reviewer")).await.unwrap();

        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Sub(sub),
            agent_type: Some("code-reviewer".to_string()),
            unattended: false,
            session: session.clone(),
            frames: test_frames(),
            last_client: Mutex::new(None),
        };
        let contexts = provider.provide().await.expect("contexts");
        let prompt = contexts.system_prompt.unwrap_or_default();
        assert!(
            prompt.contains("# Subagent role: code-reviewer"),
            "the plugin's agent names the role: {prompt}"
        );
        assert!(
            prompt.contains("Report only high-confidence bugs."),
            "the plugin's body is the role: {prompt}"
        );
        // `Read, Grep` in Claude's vocabulary is `read_file, grep` in horsie's.
        let tools: Vec<String> = contexts
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
        assert!(tools.contains(&"grep".to_string()), "{tools:?}");
        assert!(
            !tools.contains(&"bash".to_string()),
            "the allowlist must exclude what it did not name: {tools:?}"
        );
    }

    /// The definition is resolved when the subagent runs, not carried from the
    /// spawn — so an agent whose plugin has gone fails loudly rather than
    /// running a prompt nobody can point at.
    #[tokio::test]
    async fn a_subagent_whose_agent_type_is_gone_fails_rather_than_running_generic() {
        let (f, session, id) = agent_harness().await;
        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Sub(Uuid::new_v4()),
            agent_type: Some("uninstalled-agent".to_string()),
            unattended: false,
            session: session.clone(),
            frames: test_frames(),
            last_client: Mutex::new(None),
        };
        let Err(err) = provider.provide().await else {
            panic!("a subagent whose agent type is gone must not run generic");
        };
        assert!(err.message.contains("uninstalled-agent"), "{}", err.message);
        assert!(
            !err.terminal,
            "a missing plugin is not the end of a session"
        );
    }

    /// The type is what `SubagentStart` / `SubagentStop` matchers select on. It
    /// was the constant `"subagent"` for every subagent before agent types
    /// existed, so a matcher could only select all or none.
    #[tokio::test]
    async fn the_agent_type_reaches_the_subagent_hook_matcher() {
        let (f, session, _id) = agent_harness().await;
        spawn_typed(&session, Some("code-reviewer")).await.unwrap();
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let types: Vec<String> = f
            .agent
            .server_hook_events()
            .into_iter()
            .filter_map(|e| match e {
                horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
                _ => None,
            })
            .collect();
        assert_eq!(types, vec!["code-reviewer".to_string()]);
    }

    /// An untyped spawn is the general-purpose subagent, unchanged.
    #[tokio::test]
    async fn an_untyped_spawn_still_reports_the_generic_type() {
        let (f, session, _id) = agent_harness().await;
        spawn_typed(&session, None).await.unwrap();
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let types: Vec<String> = f
            .agent
            .server_hook_events()
            .into_iter()
            .filter_map(|e| match e {
                horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
                _ => None,
            })
            .collect();
        assert_eq!(types, vec!["subagent".to_string()]);
    }

    /// A halt from a tool hook reaches the session as its own command, because
    /// the runtime that ran the hook cannot end a turn and the agent is mid-call
    /// when it arrives. The reason is what the user is shown.
    #[tokio::test]
    async fn halting_the_main_agent_fails_the_turn_with_the_hooks_reason() {
        let gate = BlockingProvider::new();
        let (_f, session, _id, _journal) = spawn_session_with_provider(gate.clone()).await;
        let status = |s: ActorRef<SessionCommand>| async move {
            s.ask(|reply| SessionCommand::Snapshot { reply })
                .await
                .unwrap()
                .status
        };
        send(&session, "go").await;
        // The turn is parked in the provider, which is where a tool hook's halt
        // would arrive from.
        for _ in 0..200 {
            if status(session.clone()).await == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        session
            .tell(SessionCommand::HaltAgent {
                key: AgentKey::Main,
                reason: "the repo is locked".into(),
            })
            .await
            .unwrap();
        gate.release();

        for _ in 0..200 {
            if let SessionStatus::Failed { reason } = status(session.clone()).await {
                assert_eq!(reason, "the repo is locked");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the halted turn never failed");
    }

    /// `Stop` runs after the fact, so a guard that could not run cannot deny
    /// anything. Only `PreToolUse` fails closed.
    #[tokio::test]
    async fn a_failing_stop_hook_concludes_the_turn_anyway() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Failed(
            horsie_models::hooks::HookFailed {
                reason: "spawn failed".into(),
            },
        ))]])
        .await;
        send(&session, "go").await;
        assert_eq!(settled_inputs(&session).await.len(), 1);
    }

    /// Every `Stop` hook that ran reaches the transcript, which is the point of
    /// running them at all.
    #[tokio::test]
    async fn every_stop_hook_run_reaches_the_transcript() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
            horsie_models::hooks::ContextInjected {
                additional_context: None,
            },
        ))]])
        .await;
        send(&session, "go").await;
        settled_inputs(&session).await;
        assert_eq!(stop_outcomes(&session).await.len(), 1);
    }

    /// The bug this change exists to close, end to end.
    ///
    /// `injected_context` knew how to pull `additionalContext` off a `Stop`
    /// record and had exactly one caller — the `SessionStart` bootstrap — so a
    /// `Stop` hook's context was recorded, rendered in the web UI, and never
    /// shown to the model. It reaches the next turn's prompt now because
    /// `prompt_messages` translates the record where it sits.
    #[tokio::test]
    async fn a_stop_hooks_context_reaches_the_next_prompt() {
        let (_f, session, prompts) = stop_harness_with_prompts(vec![vec![stop_record(
            StopOutcome::Ran(horsie_models::hooks::ContextInjected {
                additional_context: Some("run the linter before you finish".into()),
            }),
        )]])
        .await;
        send(&session, "first").await;
        settled_inputs(&session).await;
        assert_eq!(stop_outcomes(&session).await.len(), 1);

        // The hook ran when the first turn ended, so the second turn is the
        // first prompt that can carry it.
        send(&session, "second").await;
        settled_inputs(&session).await;

        let seen = prompts.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|t| t.contains("run the linter before you finish")),
            "the Stop hook's context must reach the model, got {seen:?}"
        );
    }

    // --- Start hooks, and which event a turn actually fires ---
    //
    // Deciding *which* event fires, and how often, is this layer's job: the
    // agent only says "a start is due, on this source" and "here is the prompt".
    // The prompt those records reach is `hook_translation`'s job, tested there.

    /// `SessionStart` used to fire from `provide()`, which is per-run — so every
    /// turn re-ran every start hook, always reporting `source: "startup"`. It
    /// fires once per agent load now; `UserPromptSubmit` is the one that belongs
    /// to every turn.
    #[tokio::test]
    async fn a_session_starts_once_but_every_prompt_is_hooked() {
        let (f, session) = stop_harness(vec![]).await;
        send(&session, "first").await;
        settled_inputs(&session).await;
        send(&session, "second").await;
        settled_inputs(&session).await;

        let starts = f
            .agent
            .hook_events()
            .into_iter()
            .filter(|e| *e == "SessionStart")
            .count();
        let prompts = f
            .agent
            .hook_events()
            .into_iter()
            .filter(|e| *e == "UserPromptSubmit")
            .count();
        assert_eq!(starts, 1, "the start hook is due once per agent load");
        assert_eq!(prompts, 2, "the prompt hook is due every turn");
    }

    /// A subagent is not a session. The call fired `SessionStart` for one before
    /// this, because it was not gated on the agent's kind at all — so a hook
    /// matching `startup` fired again for every subagent spawned.
    #[tokio::test]
    async fn a_subagent_fires_subagent_start_never_session_start() {
        let (f, session) = stop_harness(vec![]).await;
        spawn_sub(&session, "research", "dig into it").await;
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The main agent runs a turn of its own once the subagent reports back,
        // so one `SessionStart` is correct here. What must never happen is the
        // subagent contributing a second one — which is what it did before,
        // because the call was not gated on the agent's kind.
        let events = f.agent.hook_events();
        assert_eq!(
            events.iter().filter(|e| **e == "SubagentStart").count(),
            1,
            "the subagent starts as a subagent, got {events:?}"
        );
        assert_eq!(
            events.iter().filter(|e| **e == "SessionStart").count(),
            1,
            "only the session's own agent may claim a session start, got {events:?}"
        );
    }

    /// A hook guards one agent's call, so its record belongs in that agent's
    /// transcript. Routed to the session instead, every agent's hooks would pile
    /// into one log with no way to tell whose call they guarded — which is what
    /// the session-scoped journal did before.
    #[tokio::test]
    async fn a_subagents_hooks_land_on_its_own_transcript() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;

        session
            .tell(SessionCommand::HooksRan {
                key: AgentKey::Sub(sub),
                records: vec![hook_record("guard", "tc1")],
            })
            .await
            .unwrap();

        // `tell` is fire-and-forget through two mailboxes; poll rather than race.
        let mut waited = 0;
        loop {
            let page = agent_history(&session, Some(sub.to_string())).await;
            if !hook_ids(&page).is_empty() {
                assert_eq!(hook_ids(&page), vec!["hook:0".to_string()]);
                break;
            }
            assert!(waited < 100, "the subagent never recorded the hook");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }

        let main = agent_history(&session, None).await;
        assert!(
            hook_ids(&main).is_empty(),
            "the main agent made no such call: {:?}",
            main.entries
        );
    }

    async fn main_history(session: &ActorRef<SessionCommand>) -> horsie_workflow::AgentHistoryPage {
        session
            .ask(|reply| SessionCommand::History {
                agent_id: None,
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    after: None,
                    limit: 50,
                },
                reply,
            })
            .await
            .unwrap()
            .expect("main agent history")
    }

    #[tokio::test]
    async fn a_completed_subagent_notifies_an_idle_main_agent() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        // Owed, then delivered: the tree's notified flag flips exactly once.
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        let texts = subagent_texts(&main_history(&session).await);
        assert!(
            texts.iter().any(
                |t| t.contains("[subagent \"research\" completed]") && t.contains("sub answer")
            ),
            "the main agent must be told the result: {texts:?}"
        );
        // The result is a part of its own, not text merged into the user's
        // message: that separation is what lets a client render it as agent
        // work instead of as something the person typed.
        assert!(
            user_texts(&main_history(&session).await)
                .iter()
                .all(|t| !t.contains("[subagent ")),
            "a result must never land in the user text"
        );
    }

    /// Fails any completion whose conversation contains `needle`; answers
    /// everything else with plain text. Distinguishes the subagent's run from
    /// the main agent's when both share one provider.
    struct FailOnNeedleProvider {
        needle: String,
    }

    #[async_trait]
    impl LlmProvider for FailOnNeedleProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            let hit = request
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .any(|p| matches!(p, horsie_agentcore::ContentPart::Text(t) if t.text.contains(&self.needle)));
            if hit {
                return Err(horsie_agentcore::LlmError::ApiError {
                    status: 401,
                    message: "bad key".to_string(),
                });
            }
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "fine".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    #[tokio::test]
    async fn a_failed_subagent_reports_the_error_to_its_parent() {
        let provider = FailOnNeedleProvider {
            needle: "doomed task".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        let sub = spawn_sub(&session, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = state.mode.subagents().get(&sub).unwrap();
        assert_eq!(
            rec.status,
            crate::sessions::subagents::SubAgentStatus::Failed
        );
        assert!(rec.error.as_deref().unwrap().contains("bad key"));
        let texts = subagent_texts(&main_history(&session).await);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]")),
            "the parent must hear the failure: {texts:?}"
        );
    }

    #[tokio::test]
    async fn a_notification_waits_out_an_awaiting_input_session() {
        use horsie_agentcore::StopReason;
        use horsie_agentcore::testkit::{MockProvider, Script};
        // Main's first call asks the user; every later call (the subagent's
        // run, then the main agent's answer turn) ends with plain text.
        let provider = MockProvider::scripted(
            Script::of([Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "ask-1".into(),
                        name: "ask_user".into(),
                        input: serde_json::json!({"question": "which one?"}),
                    },
                )],
                stop_reason: StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })])
            .then_repeating_with(|| {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "sub answer".to_string(),
                        },
                    )],
                    stop_reason: StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            }),
        );
        let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

        // Park the session on the ask.
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "start".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(&journal, id).await;
            if matches!(
                state.status,
                crate::sessions::spec::SessionStatus::AwaitingInput { .. }
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub).is_some_and(|r| {
                r.status == crate::sessions::subagents::SubAgentStatus::Completed && !r.notified
            })
        })
        .await;
        // The ask is still pending — the notification must not have answered it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(matches!(
            state.status,
            crate::sessions::spec::SessionStatus::AwaitingInput { .. }
        ));

        // The user's reply carries the notification along in the same input.
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "the first one".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        // A plain message does not answer the question — it abandons it and
        // starts a fresh turn — so the reply and the notification ride in the
        // *user message*, while the abandoned ask gets a result of its own.
        let page = main_history(&session).await;
        let (results, texts): (Vec<String>, Vec<String>) = {
            let mut results = Vec::new();
            let mut texts = Vec::new();
            for part in page.messages().flat_map(|m| m.parts.iter()) {
                match part {
                    horsie_agentcore::ContentPart::ToolResult(r) => results.push(r.output.clone()),
                    horsie_agentcore::ContentPart::Text(t) => texts.push(t.text.clone()),
                    horsie_agentcore::ContentPart::ToolCall(_)
                    | horsie_agentcore::ContentPart::Thinking(_)
                    | horsie_agentcore::ContentPart::SubAgentResult(_) => {}
                }
            }
            (results, texts)
        };
        // One turn, two kinds of content: the person's words stay the user
        // text, the subagent's report rides alongside as its own part.
        assert!(
            texts.iter().any(|t| t.contains("the first one")),
            "the user's own message must survive the turn: {texts:?}"
        );
        let reports = subagent_texts(&main_history(&session).await);
        assert!(
            reports
                .iter()
                .any(|t| t.contains("[subagent \"research\" completed]")),
            "the notification rides the same turn: {reports:?}"
        );
        assert!(
            results.iter().any(|r| r.contains("not answered")),
            "the abandoned ask still gets a result, so nothing dangles: {results:?}"
        );
    }

    #[tokio::test]
    async fn a_stranded_grandchild_result_flushes_at_the_next_turn_boundary() {
        use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
        // Fold a crashed-session state straight into the journal: P completed
        // and its parent was told; P's child C died mid-run and was reconciled
        // to failed. Every node is terminal, so no subagent outcome will ever
        // arrive again — C's result is owed to P forever unless a turn
        // boundary delivers it.
        let p = Uuid::new_v4();
        let c = Uuid::new_v4();
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let pid = SessionActor::persistence_id_for(id);
        let events: Vec<Vec<u8>> = [
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 0,
                id: p,
                parent: SubAgentParent::Main,
                label: "parent".into(),
                task: "parent task".into(),
                depth: 1,
                agent_type: None,
            },
            SessionDomainEvent::SubAgentCompleted {
                at_ms: 0,
                id: p,
                output: "parent first answer".into(),
            },
            SessionDomainEvent::SubAgentNotified { at_ms: 0, id: p },
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 0,
                id: c,
                parent: SubAgentParent::SubAgent(p),
                label: "child".into(),
                task: "child task".into(),
                depth: 2,
                agent_type: None,
            },
            SessionDomainEvent::SubAgentFailed {
                at_ms: 0,
                id: c,
                error: crate::sessions::subagents::INTERRUPTED_ERROR.into(),
            },
        ]
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap())
        .collect();
        journal.persist(&pid, &events).await.unwrap();

        // Loading must start no runs: C stays owed until someone acts.
        let parent = spawn_deaf_supervisor();
        let session2 = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                _f.deps.clone(),
                parent,
                test_frames(),
            ),
            journal.clone(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(!state.mode.subagents().get(&c).unwrap().notified);
        assert_eq!(
            state.mode.subagents().get(&p).unwrap().status,
            SubAgentStatus::Completed
        );

        // The next turn boundary wakes P with C's failure; P concludes again
        // and its new output is owed to the main agent.
        session2
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        // P's re-completion and its notification to the main agent persist in
        // one effect, so don't wait on a `!notified` window — C delivered and
        // P re-concluded are the durable facts.
        wait_for_tree(&journal, id, |t| {
            t.get(&c).is_some_and(|r| r.notified)
                && t.get(&p).is_some_and(|r| {
                    r.status == SubAgentStatus::Completed
                        && r.output.as_deref() == Some("sub answer")
                })
        })
        .await;
        let page = session2
            .ask(|reply| SessionCommand::History {
                agent_id: Some(p.to_string()),
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    after: None,
                    limit: 20,
                },
                reply,
            })
            .await
            .unwrap()
            .expect("P's transcript");
        let texts = subagent_texts(&page);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"child\" failed]")
                    && t.contains("interrupted by restart")),
            "P must be woken with C's result: {texts:?}"
        );
        let _ = session;
    }

    #[tokio::test]
    async fn recovery_respawns_subagents_and_fails_interrupted_ones() {
        // First incarnation: a hanging provider keeps the subagent mid-run.
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        // Simulate process death: the last ref drops, the journal lives on.
        drop(session);

        // Second incarnation on the same journal.
        let parent = spawn_deaf_supervisor();
        let session2 = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                parent,
                test_frames(),
            ),
            journal.clone(),
        );
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Failed)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.mode.subagents().get(&sub).unwrap().error.as_deref(),
            Some(crate::sessions::subagents::INTERRUPTED_ERROR)
        );
        // The transcript stays pageable: the resident actor answers history.
        let page = session2
            .ask(|reply| SessionCommand::History {
                agent_id: Some(sub.to_string()),
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    after: None,
                    limit: 10,
                },
                reply,
            })
            .await
            .unwrap();
        assert!(page.is_some(), "a reloaded subagent must answer history");
        gate.release();
    }

    #[tokio::test]
    async fn prepare_offload_refuses_with_an_active_subagent() {
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let _sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| t.has_active()).await;

        let offloadable = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .unwrap();
        assert!(!offloadable, "an active subagent must block offload");
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime"
        );
        gate.release();
    }
}
