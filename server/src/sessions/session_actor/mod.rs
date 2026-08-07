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
//!
//! Two of its neighbours live alongside it rather than inside it: [`context`]
//! assembles one turn (runtime handle, toolbox, system prompt) on the agent's
//! own task, and [`hooks`] is the pair of sinks a plugin's hooks report
//! themselves through.

mod context;
mod hooks;

use crate::{
    runtime_manager::RuntimeError,
    sessions::{
        UserMessageError,
        ask_tool::ASK_USER_TOOL,
        orchestrator::{AgentAction, InteractiveOrchestrator, Orchestrator, SessionCommandKind},
        spec::{PendingAsk, ServerDeps, SessionSpec, SessionStatus},
        subagents::{
            INTERRUPTED_ERROR, MAX_SUBAGENT_DEPTH, SubAgentForest, SubAgentParent, SubAgentTree,
            TreeOwner,
        },
        supervisor::SessionSupervisorCommand,
        title_tool::normalize_session_title,
        workflow::WorkflowRunState,
    },
};
use async_trait::async_trait;
use context::{SessionAgentKind, SessionContextProvider, session_run_def};
use hooks::StopHookParent;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::{agent::ToolResultInput, hooks::HookRecord, now_ms};
use horsie_workflow::{
    AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRunDef, AgentRuntimeContext,
    AgentUsageSnapshot, UsageTotal,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The agent id a session's primary agent reports usage under.
const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Read forward from a cursor in one of the session's agents: `agent_id`
    /// absent or `"main"` for the primary agent, otherwise a subagent id.
    /// `None` answers "no such agent".
    ReadLog {
        agent_id: Option<String>,
        after: Option<horsie_workflow::Cursor>,
        reply: oneshot::Sender<Option<horsie_workflow::ReadOutcome>>,
    },
    /// Read a window *backwards* from a cursor — scroll-back.
    PageLog {
        agent_id: Option<String>,
        before: Option<u64>,
        max: usize,
        reply: oneshot::Sender<Option<horsie_workflow::LogPage>>,
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
    /// Record one turn-preparation stage in `key`'s log. Sent by the context
    /// provider as it assembles a turn.
    Progress {
        key: AgentKey,
        stage: String,
        detail: Option<String>,
    },
    /// Read one agent's current values (task list, usage) for its document.
    AgentState {
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<horsie_workflow::AgentStateView>>,
    },
    /// Build this session's runtime.
    ///
    /// Sent once, by the supervisor, as part of creating the session — and
    /// again by the session itself when it loads to find a create that the
    /// process died inside. It is idempotent against a runtime that already
    /// exists: a session that is past provisioning ignores it, which is what
    /// keeps "provisioned exactly once" true without any bookkeeping beyond the
    /// status the journal already carries.
    Provision,
    /// Internal: the detached create finished. Carries the vendor's own error
    /// rather than a summary, because that string is what the user is shown.
    FinishProvisioning {
        error: Option<String>,
        terminal: bool,
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
    /// This session's runtime is being built. Journaled *before* the vendor is
    /// called, which is the whole of the fix for a first turn outrunning its
    /// own create: the status it produces starts nothing, so a message that
    /// arrives meanwhile queues instead of asking a vendor that has never
    /// heard of the runtime.
    ///
    /// Finding this unfinished at load means the process died mid-create. That
    /// is safe to re-attempt precisely because no turn can have run under it.
    ProvisioningStarted {
        at_ms: u64,
    },
    /// The vendor confirmed the runtime. The session becomes ordinary here,
    /// and whatever queued behind the create starts.
    ProvisioningSucceeded {
        at_ms: u64,
    },
    /// The create failed. `terminal` carries the one distinction that matters:
    /// a live vendor refusing to produce the runtime ends the session, while an
    /// offline vendor or a failed token mint leaves it retryable.
    ProvisioningFailed {
        at_ms: u64,
        error: String,
        terminal: bool,
    },
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
#[serde(default, from = "SessionStateWire")]
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
    /// The workflow run this session is, if it is one. `None` for every
    /// conversation, which is what makes the field additive.
    #[serde(default)]
    pub run: Option<WorkflowRunState>,
    /// Every subagent this session holds, in one forest keyed by the agent that
    /// roots each tree. Beside the run rather than inside it: a capability that
    /// lives inside one kind is a capability the other kind silently loses.
    #[serde(default)]
    pub subagents: SubAgentForest,
}

impl SessionState {
    /// What this session's own "Main" means right now: the step in flight for a
    /// run, the main agent otherwise. The single kind-shaped fact the subagent
    /// code is ever told, and it arrives as a value rather than a branch.
    pub fn root_owner(&self) -> TreeOwner {
        match self.run.as_ref().and_then(WorkflowRunState::current_agent) {
            Some(agent) => TreeOwner::Step(agent),
            None => TreeOwner::Main,
        }
    }
}

/// Every snapshot shape `SessionState` has ever been written in.
///
/// Three, because the subagent tree has moved twice: it was a bare field, then
/// nested under `mode`, and is now a forest beside the run. A snapshot that
/// fails to deserialize is a session that cannot be opened at all, so each
/// older shape is read rather than rejected. Only the newest is ever written.
#[derive(Deserialize)]
#[serde(default)]
struct SessionStateWire {
    status: SessionStatus,
    pending_asks: Vec<PendingAsk>,
    inbox: Vec<InboxMessage>,
    last_error: Option<String>,
    agent_usage: HashMap<String, UsageTotal>,
    run: Option<WorkflowRunState>,
    /// A forest (current) or a bare tree (pre-`mode`). One key has held both
    /// shapes, so it is read through an untagged enum rather than two fields.
    subagents: Option<LegacySubagents>,
    mode: Option<LegacyMode>,
}

impl Default for SessionStateWire {
    fn default() -> Self {
        Self {
            status: SessionStatus::Idle,
            pending_asks: Vec::new(),
            inbox: Vec::new(),
            last_error: None,
            agent_usage: HashMap::new(),
            run: None,
            subagents: None,
            mode: None,
        }
    }
}

/// The `subagents` key has meant two things. Forest first: a current snapshot
/// must never be misread as a tree, and `SubAgentTree`'s `nodes` map would
/// otherwise happily accept an empty object.
#[derive(Deserialize)]
#[serde(untagged)]
enum LegacySubagents {
    Forest(SubAgentForest),
    Tree(SubAgentTree),
}

/// The `mode`-tagged shape: a conversation carried `subagents`, a run carried
/// `run` with one tree per step.
#[derive(Deserialize)]
struct LegacyMode {
    #[serde(default)]
    subagents: SubAgentTree,
    #[serde(default)]
    run: Option<LegacyRun>,
}

/// The run as it was written, spelled out rather than flattened. A
/// `#[serde(flatten)]` of `WorkflowRunState` beside an explicit `steps` loses
/// the steps: serde fills the named field and the flattened struct never sees
/// the key.
#[derive(Deserialize)]
struct LegacyRun {
    #[serde(default)]
    status: crate::sessions::workflow::WorkflowRunStatus,
    #[serde(default)]
    steps: Vec<LegacyStepRun>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// One step execution plus the tree it used to carry. `subagents` is no longer
/// a field of `StepRun`, so the flatten leaves it here.
#[derive(Deserialize)]
struct LegacyStepRun {
    #[serde(flatten)]
    step: crate::sessions::workflow::StepRun,
    #[serde(default)]
    subagents: SubAgentTree,
}

impl From<SessionStateWire> for SessionState {
    fn from(w: SessionStateWire) -> Self {
        // Newest first, so a current snapshot never pays for the legacy paths.
        let (run, subagents) = match (w.subagents, w.mode) {
            (Some(LegacySubagents::Forest(forest)), _) => (w.run, forest),
            (legacy_tree, mode) => {
                let mut forest = SubAgentForest::default();
                let mut run = w.run;
                if let Some(mode) = mode {
                    if let Some(legacy) = mode.run {
                        // One tree per step, keyed by that step's agent id.
                        let mut steps = Vec::with_capacity(legacy.steps.len());
                        for entry in legacy.steps {
                            if !entry.subagents.is_empty() {
                                *forest.tree_mut(TreeOwner::Step(entry.step.agent)) =
                                    entry.subagents;
                            }
                            steps.push(entry.step);
                        }
                        run = Some(WorkflowRunState {
                            status: legacy.status,
                            steps,
                            output: legacy.output,
                            error: legacy.error,
                        });
                    } else if !mode.subagents.is_empty() {
                        *forest.tree_mut(TreeOwner::Main) = mode.subagents;
                    }
                } else if let Some(LegacySubagents::Tree(tree)) = legacy_tree
                    && !tree.is_empty()
                {
                    *forest.tree_mut(TreeOwner::Main) = tree;
                }
                (run, forest)
            }
        };
        Self {
            status: w.status,
            pending_asks: w.pending_asks,
            inbox: w.inbox,
            last_error: w.last_error,
            agent_usage: w.agent_usage,
            run,
            subagents,
        }
    }
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
    /// The supervisor's per-agent position channels for this session.
    ///
    /// Cloned in rather than created here: it has to outlive this actor, so
    /// that unloading an idle session leaves a reader waiting rather than
    /// disconnecting it.
    positions: crate::sessions::Positions,
}

impl SessionActor {
    pub fn new(
        id: Uuid,
        spec: SessionSpec,
        deps: ServerDeps,
        parent: ActorRef<SessionSupervisorCommand>,
        positions: crate::sessions::Positions,
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
            agents: None,
            pending_step: None,
            orchestrator,
            context_provider: None,
            positions,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor's cache and the live stream.
    async fn report(&self, status: SessionStatus) {
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status,
            })
            .await;
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
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
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
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            position: self.positions.for_agent(MAIN_AGENT_ID),
            parent: StopHookParent::wrap(ctx.self_ref(), AgentKey::Main, context_provider.clone()),
            session_id: self.id,
        };
        self.agents = Some(SessionAgents::interactive(
            ctx.spawn(AgentActor::new(agent_ctx, params)),
        ));
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
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
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
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            position: self.positions.for_agent(&agent_id.to_string()),
            parent: StopHookParent::wrap(
                ctx.self_ref(),
                AgentKey::Step(agent_id),
                context_provider.clone(),
            ),
            session_id: agent_id,
        };
        let actor = ctx.spawn(AgentActor::new(agent_ctx, params));
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
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "step_started".into(),
                            detail: Some(step.clone()),
                        },
                    ),
                )
                .await;
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

    /// Tell each agent about the session events it needs to show.
    ///
    /// This is the whole of "session events reach the client": the session
    /// still owns and journals every one of them, and hands the viewer-facing
    /// subset to the agent whose log a person would be reading. One direction
    /// only — an agent never tells the session anything back through here.
    ///
    /// **Resident agents only**, because this hook has no `ActorContext` to
    /// spawn with. That is not the limitation it looks like: `main` is spawned
    /// at recovery and stays for the session's loaded life, and every
    /// subagent-targeted event happens while that subagent is running. A miss
    /// is therefore a bug worth hearing about rather than a case to handle,
    /// which is what the warning is for.
    async fn record_lifecycle(&mut self, events: &[SessionDomainEvent]) {
        for event in events {
            let (target, Some(payload)) = crate::sessions::lifecycle_routing::route(event) else {
                continue;
            };
            let agent = match &target {
                crate::sessions::lifecycle_routing::LifecycleTarget::None => continue,
                crate::sessions::lifecycle_routing::LifecycleTarget::Main => {
                    self.agents.as_ref().and_then(SessionAgents::main).cloned()
                }
                crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Main) => {
                    self.agents.as_ref().and_then(SessionAgents::main).cloned()
                }
                crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Sub(id))
                | crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Step(id)) => {
                    self.agents.as_ref().and_then(|a| a.sub(*id)).cloned()
                }
            };
            let Some(agent) = agent else {
                tracing::warn!(
                    session = %self.id,
                    ?target,
                    "no resident agent to record a session event on; it will be missing from the log"
                );
                continue;
            };
            let _ = agent
                .tell(AgentCommand::RecordLifecycle {
                    event: payload,
                    at_ms: now_ms(),
                })
                .await;
        }
    }

    /// Record one lifecycle entry on a named agent, when it is resident.
    async fn record_on(&mut self, key: AgentKey, event: horsie_agentcore::LifecycleEvent) {
        let agent = match key {
            AgentKey::Main => self.agents.as_ref().and_then(SessionAgents::main).cloned(),
            AgentKey::Sub(id) | AgentKey::Step(id) => {
                self.agents.as_ref().and_then(|a| a.sub(id)).cloned()
            }
        };
        if let Some(agent) = agent {
            let _ = agent
                .tell(AgentCommand::RecordLifecycle {
                    event,
                    at_ms: now_ms(),
                })
                .await;
        }
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
                let agent_type = state.subagents.node(id)?.agent_type.clone();
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
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
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
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            position: self.positions.for_agent(&id.to_string()),
            parent: StopHookParent::wrap(
                ctx.self_ref(),
                AgentKey::Sub(id),
                context_provider.clone(),
            ),
            session_id: id,
        };
        let actor = ctx.spawn(AgentActor::new(agent_ctx, params));
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
                    None => match state.subagents.node(id) {
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
        // A session whose create failed has no runtime, so the message that the
        // UI invited ("send a message to try again") has to build one rather
        // than start a turn that would ask for it. The message stays queued and
        // the create's own completion drains it, exactly as at session creation.
        if matches!(next.status, SessionStatus::ProvisioningFailed { .. }) {
            let _ = ctx.self_ref().tell(SessionCommand::Provision).await;
        } else {
            events.extend(self.flush_then_drain(&next, ctx).await);
        }
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
        if let Some(run) = state.run.as_ref() {
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
        let Some(run) = state.run.as_ref() else {
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
            .run
            .as_ref()
            .map(|r| r.steps.len() as u32)
            .unwrap_or_default();
        let attempt = next
            .run
            .as_ref()
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
            .run
            .as_ref()
            .and_then(|r| r.get(index))
            .map(|s| s.step.clone())
            .unwrap_or_default();
        let (mut events, advance) = match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { output, .. } => {
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "step_concluded".into(),
                            detail: Some(step_name),
                        },
                    ),
                )
                .await;
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
        let Some(rec) = state.subagents.node(id).cloned() else {
            tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored");
            return CommandEffect::none();
        };
        let terminal = match outcome {
            AgentOutcome::Concluded { output, .. } => {
                let text = output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string());
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "subagent_completed".into(),
                            detail: Some(format!("\"{}\" ({id})", rec.label)),
                        },
                    ),
                )
                .await;
                SessionDomainEvent::SubAgentCompleted {
                    at_ms: now_ms(),
                    id,
                    output: text,
                }
            }
            AgentOutcome::Failed { error, .. } => {
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "subagent_failed".into(),
                            detail: Some(format!("\"{}\" ({id})", rec.label)),
                        },
                    ),
                )
                .await;
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
            SessionDomainEvent::ProvisioningStarted { .. } => {
                state.status = SessionStatus::Provisioning;
            }
            SessionDomainEvent::ProvisioningSucceeded { .. } => {
                state.status = SessionStatus::Idle;
                state.last_error = None;
            }
            SessionDomainEvent::ProvisioningFailed {
                error, terminal, ..
            } => {
                state.status = if terminal {
                    SessionStatus::Unrecoverable {
                        reason: error.clone(),
                    }
                } else {
                    SessionStatus::ProvisioningFailed {
                        reason: error.clone(),
                    }
                };
                state.last_error = Some(error);
            }
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
                if let Some(run) = state.run.as_mut() {
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
                // The owner is resolved against the state as it stands *before*
                // this event: the step in flight for a run, Main otherwise.
                let owner = state
                    .subagents
                    .owner_for(parent, state.root_owner())
                    .unwrap_or(TreeOwner::Main);
                state
                    .subagents
                    .tree_mut(owner)
                    .apply_spawned(id, parent, label, task, depth, at_ms, agent_type);
            }
            SessionDomainEvent::SubAgentRunning { id, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_running(id, at_ms);
                }
            }
            SessionDomainEvent::SubAgentCompleted { id, output, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_completed(id, output, at_ms);
                }
            }
            SessionDomainEvent::SubAgentFailed { id, error, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_failed(id, error, at_ms);
                }
            }
            SessionDomainEvent::SubAgentNotified { id, .. } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_notified(id);
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
                // The first step is what turns this state into a run:
                // `initial_state` is static and cannot see the spec, so the run
                // is established by the log rather than at construction.
                let run = state.run.get_or_insert_with(WorkflowRunState::default);
                {
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
                if let Some(run) = state.run.as_mut() {
                    run.apply_concluded(index, output, at_ms);
                }
            }
            SessionDomainEvent::StepFailed {
                at_ms,
                index,
                error,
            } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_step_failed(index, error.clone(), at_ms);
                    run.apply_failed(error.clone());
                }
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::StepCancelled { at_ms, index } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_cancelled(index, at_ms);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFinished { output, .. } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_finished(output);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFailed { error, .. } => {
                if let Some(run) = state.run.as_mut() {
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

    /// Write what just became durable into the agents' own transcripts, so a
    /// reader sees a lifecycle entry where it happened rather than having to
    /// infer it from the session's status.
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], _state: &SessionState) {
        self.record_lifecycle(events).await;
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
            SessionCommand::ReadLog {
                agent_id,
                after,
                reply,
            } => {
                // Read from the resident actor's in-memory state. No journal
                // access, no runtime — opening a session to read it stays free
                // of sandbox cost.
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let out = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::ReadLog { after, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(out);
                CommandEffect::none()
            }
            SessionCommand::PageLog {
                agent_id,
                before,
                max,
                reply,
            } => {
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let page = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::PageLog { before, max, reply })
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
            SessionCommand::Progress { key, stage, detail } => {
                self.record_on(
                    key,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle { stage, detail },
                    ),
                )
                .await;
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
            SessionCommand::Provision => {
                // Provision only from the three states that mean "no runtime has
                // ever been confirmed": a session just created (nothing
                // journaled, so the default `Idle`), one found still
                // `Provisioning` at load because the process died inside its
                // create, and one whose create failed on something retryable.
                //
                // Every other status means a create already succeeded, and
                // re-running one would rebuild a workspace someone may be using
                // — the thing this design exists to make impossible. The
                // `Idle` arm is the loose one: it is also every healthy
                // session's status, so it holds only because the supervisor
                // sends this exactly once, at creation.
                if !matches!(
                    state.status,
                    SessionStatus::Idle
                        | SessionStatus::Provisioning
                        | SessionStatus::ProvisioningFailed { .. }
                ) {
                    return CommandEffect::none();
                }
                let runtimes = self.deps.runtimes.clone();
                let session = self.id.to_string();
                let vendor = self.spec.vendor.clone();
                let spec = self.spec.clone();
                let me = ctx.self_ref();
                // Off the mailbox: a real create runs for minutes, and this
                // actor has to keep answering reads, stops and deletes
                // throughout. The status it just journaled is what holds the
                // turn back meanwhile.
                tokio::spawn(async move {
                    let (error, terminal) = match runtimes.create(&session, &vendor, &spec).await {
                        Ok(()) => (None, false),
                        // Exactly the split `get` makes: only a live vendor
                        // refusing to produce the runtime is terminal. An
                        // offline vendor or a failed token mint is a bad
                        // moment, not a dead session.
                        Err(e @ RuntimeError::Gone(_)) => (Some(e.to_string()), true),
                        Err(e @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_))) => {
                            (Some(e.to_string()), false)
                        }
                    };
                    let _ = me
                        .tell(SessionCommand::FinishProvisioning { error, terminal })
                        .await;
                });
                self.report(SessionStatus::Provisioning).await;
                CommandEffect::persist(vec![SessionDomainEvent::ProvisioningStarted {
                    at_ms: now_ms(),
                }])
            }
            SessionCommand::FinishProvisioning { error, terminal } => {
                let event = match error {
                    None => SessionDomainEvent::ProvisioningSucceeded { at_ms: now_ms() },
                    Some(error) => SessionDomainEvent::ProvisioningFailed {
                        at_ms: now_ms(),
                        error,
                        terminal,
                    },
                };
                let next = Self::apply_event(state.clone(), event.clone());
                self.report(next.status.clone()).await;
                let mut events = vec![event];
                // The runtime landed, so whatever queued behind it starts now.
                // A failure drains nothing: the messages stay owed, and the
                // next thing the user sends is what tries again.
                events.extend(self.flush_then_drain(&next, ctx).await);
                CommandEffect::persist(events)
            }
            SessionCommand::PrepareOffload { reply } => {
                // A run started while the supervisor was deciding: refuse, and
                // let the idle clock start again. This is the invariant that
                // keeps a forty-minute tool call from being unloaded out from
                // under itself — the main agent's run, or any subagent's.
                if matches!(
                    state.status,
                    SessionStatus::Running | SessionStatus::Provisioning
                ) || state.subagents.has_active()
                {
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
                let _ = reply.send(state.run.clone());
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
                // Every tree, not one: the API reports a run's step subagents
                // alongside a conversation's.
                let tree = state
                    .subagents
                    .ids()
                    .into_iter()
                    .filter_map(|id| state.subagents.node(id).map(|rec| (id, rec.clone())))
                    .collect();
                let _ = reply.send(tree);
                CommandEffect::none()
            }
            SessionCommand::ReconcileSubAgents => {
                let interrupted = state.subagents.interrupted();
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
                let owner = state.subagents.owner_for(caller, state.root_owner());
                let Some(parent_depth) = owner
                    .and_then(|owner| state.subagents.tree(owner))
                    .map_or_else(
                        // An empty forest still has a Main at depth 0: the very
                        // first spawn of a session has no tree to look in yet.
                        || matches!(caller, SubAgentParent::Main).then_some(0),
                        |tree| tree.depth_of(caller),
                    )
                else {
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
                if state.subagents.active_count() >= max {
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
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "subagent_spawned".into(),
                            detail: Some(format!("\"{label}\" ({id})")),
                        },
                    ),
                )
                .await;
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SessionCommand::SubAgentStatus { caller, id, reply } => {
                // Visibility is answered within the caller's own tree: a step
                // and a conversation each see their own, and neither learns the
                // other exists.
                let tree = state
                    .subagents
                    .owner_for(caller, state.root_owner())
                    .and_then(|owner| state.subagents.tree(owner));
                let rendered = match id {
                    Some(id) if tree.is_some_and(|t| t.visible_to(caller, &id)) => tree
                        .and_then(|t| t.render_node(&id))
                        .ok_or_else(|| format!("no such subagent: {id}")),
                    // Out-of-subtree and unknown ids are indistinguishable —
                    // neither confirms the node exists.
                    Some(id) => Err(format!("no such subagent: {id}")),
                    None => Ok(tree
                        .map(|t| t.render_subtree(caller))
                        .unwrap_or_else(|| "No subagents.\n".to_string())),
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
            let unstarted = match state.run.as_ref() {
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
        if !state.subagents.interrupted().is_empty() {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileSubAgents)
                .await;
        }
        // A create the process died inside. Finishing it is the whole reason
        // provisioning is journaled rather than held in a map beside the
        // runtime manager: nothing else can know a create was ever started.
        // Re-attempting is safe here and nowhere else — `Provisioning` is
        // precisely the state in which no turn has run, so there is no work in
        // the workspace for a rebuild to destroy.
        // …and a create that answered with a retryable failure, which is the
        // only retry a workflow run can ever get: a run takes no messages, so
        // the message-shaped retry cannot reach one.
        if matches!(
            state.status,
            SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
        ) {
            let _ = ctx.self_ref().tell(SessionCommand::Provision).await;
            return;
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
mod tests;
