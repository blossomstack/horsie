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

mod component;
mod context;
mod core;
mod hooks;
mod lifecycle;
mod reads;
mod run;
mod subagent;
mod turns;

use component::Component;
use core::SessionCore;
use hooks::{HookRouting, StopHookParent};
use lifecycle::RuntimeLifecycle;
use reads::Reads;
use run::WorkflowRun;
use subagent::SubAgents;
use turns::Turns;

use crate::sessions::{
    UserMessageError,
    ask_tool::ASK_USER_TOOL,
    orchestrator::AgentAction,
    spec::{PendingAsk, ServerDeps, SessionSpec, SessionStatus},
    subagents::{SubAgentForest, SubAgentParent, SubAgentTree, TreeOwner},
    supervisor::SessionSupervisorCommand,
    workflow::WorkflowRunState,
};
use async_trait::async_trait;
use context::{SessionAgentKind, SessionContextProvider, session_run_def};
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::{hooks::HookRecord, now_ms};
use horsie_workflow::{
    AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRuntimeContext, AgentUsageSnapshot,
    UsageTotal,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
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
    /// Getting and releasing this session's sandbox.
    Lifecycle(LifecycleCommand),
    /// The conversation: what a person sends and how a turn ends.
    Turn(TurnCommand),
    /// The workflow graph, when this session is a run.
    Run(RunCommand),
    /// The tree of delegated work.
    SubAgent(SubAgentCommand),
    /// Questions answered without waking anything.
    Read(ReadCommand),
    /// What plugin hooks did, routed to the agent it happened to.
    Hooks(HookCommand),
    /// The session's own bookkeeping: its title, and preparation progress.
    Core(CoreCommand),
    /// Internal: an agent reported its terminal outcome. Top-level because it is
    /// the one command routed by *identity* rather than by variant — which agent
    /// sent it decides which component answers.
    AgentOutcome(AgentOutcome),
}

/// Getting and releasing this session's sandbox.
pub enum LifecycleCommand {
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
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: oneshot::Sender<()> },
}

/// The conversation.
pub enum TurnCommand {
    /// A user message. Always accepted: it is queued durably and answered by
    /// the next turn, so there is no rejection path and no `409`.
    UserMessage {
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    },
    /// Cancel the turn in flight. Queued messages are *not* discarded — stop
    /// means "not this turn", not "throw away what I asked for".
    Stop { reply: oneshot::Sender<()> },
    /// Answer every pending ask at once, resuming the turn.
    Answer {
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<Result<(), AnswerError>>,
    },
    /// Internal: post-recovery reconciliation of a turn the process died in.
    ReconcileInterrupted,
}

/// The workflow graph.
pub enum RunCommand {
    /// Let the orchestrator start whatever it wants started. Sent to a run at
    /// load so a pending one begins, and after a retry.
    Advance,
    /// Re-run one execution from the run log.
    RetryStep {
        index: u32,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Read this session's workflow run, if it is one.
    State {
        reply: oneshot::Sender<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
}

/// The tree of delegated work.
pub enum SubAgentCommand {
    /// The `spawn_agent` tool: start a subagent under `caller`.
    Spawn {
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
    /// Internal: the spawn's `SubAgentSpawned` write came back — only now
    /// does the child actor exist (persist-then-spawn). A failed write spawns
    /// nothing and the tool gets the error.
    FinishSpawn {
        id: Uuid,
        label: String,
        task: String,
        agent_type: Option<String>,
        reply: oneshot::Sender<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// The `subagent_status` tool: one node, or the caller's whole subtree.
    Status {
        caller: SubAgentParent,
        id: Option<Uuid>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Read every tree (backs `GET /api/sessions/:id/subagents`).
    Tree {
        reply: oneshot::Sender<Vec<(Uuid, crate::sessions::subagents::SubAgentRecord)>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under (tree nodes still `Running`). Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    Reconcile,
}

/// Questions answered from the resident actor's memory. None of these touches
/// the journal, so opening a session to look at it costs no sandbox.
pub enum ReadCommand {
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
    /// Read one agent's current values (task list, usage) for its document.
    AgentState {
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<horsie_workflow::AgentStateView>>,
    },
    /// Read this session's recovered state: status, pending ask, inbox.
    Snapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },
    /// Read this session's aggregated usage.
    UsageStats {
        reply: oneshot::Sender<SessionUsageStats>,
    },
}

/// What plugin hooks did. Pure routing: nothing here is persisted by the
/// session, and nothing here changes state.
pub enum HookCommand {
    /// Plugin hooks ran against one agent's tool call. The session forwards to
    /// the agent whose transcript the records belong in. Carries no reply
    /// because nothing waits on it.
    Ran {
        key: AgentKey,
        records: Vec<HookRecord>,
    },
    /// A `Stop` hook blocked, so the turn continues with `reason` as its input.
    ///
    /// Routed through the session for the same reason `Ran` is: the sink is
    /// built before its `AgentActor` is spawned, so it holds a key rather than
    /// an `ActorRef`.
    ContinueAfterStop { key: AgentKey, reason: String },
    /// A hook set `continue: false`, so this agent stops where it is.
    ///
    /// The session is the only thing that can act on it: the runtime that ran
    /// the hook has no way to end a turn, and the agent is mid-call. What
    /// stopping *means* is per key — a turn boundary for the main agent, a
    /// failed node for a subagent, a failed step for a step.
    Halt { key: AgentKey, reason: String },
}

/// The session's own bookkeeping.
pub enum CoreCommand {
    /// Set the session title from the built-in title tool.
    SetTitle {
        title: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Record one turn-preparation stage in `key`'s log. Sent by the context
    /// provider as it assembles a turn.
    Progress {
        key: AgentKey,
        stage: String,
        detail: Option<String>,
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
        Self {
            id,
            spec,
            deps,
            parent,
            agents: None,
            pending_step: None,
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

    fn agent(&self) -> Option<&ActorRef<AgentCommand>> {
        self.agents.as_ref().and_then(SessionAgents::main)
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
    /// Whether any component has work in flight, so the session must not
    /// unload. This is what keeps a forty-minute tool call from being unloaded
    /// out from under itself.
    fn busy(&self, state: &SessionState) -> bool {
        RuntimeLifecycle::busy(state)
            || Turns::busy(state)
            || WorkflowRun::busy(state)
            || SubAgents::busy(state)
    }

    /// Everything every component wants started, given the state as it now is.
    ///
    /// A concatenation, not a negotiation: each component returns only work it
    /// owns, so there is nothing to reconcile. Subagent wakes go first — a
    /// parent waiting on its children is work already in flight, and the next
    /// turn or step can wait a boundary.
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
        // Nothing starts before the runtime it would run on exists. One gate,
        // checked once, for every component.
        if !RuntimeLifecycle::ready(state) {
            return Vec::new();
        }
        let cx = component::ActionCx {
            id: self.id,
            spec: &self.spec,
        };
        [
            SubAgents::actions(&cx, state),
            Turns::actions(&cx, state),
            WorkflowRun::actions(&cx, state),
        ]
        .concat()
    }

    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for action in self.next_actions(&next) {
            let produced = self.perform(action, &next, ctx).await;
            for e in &produced {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(produced);
        }
        events
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
            SessionDomainEvent::ProvisioningStarted { .. }
            | SessionDomainEvent::ProvisioningSucceeded { .. }
            | SessionDomainEvent::ProvisioningFailed { .. } => {
                RuntimeLifecycle::apply(&mut state, &event)
            }
            SessionDomainEvent::MessageQueued { .. }
            | SessionDomainEvent::TurnBegan { .. }
            | SessionDomainEvent::AskRecorded { .. }
            | SessionDomainEvent::TurnEnded { .. }
            | SessionDomainEvent::TurnFailed { .. }
            | SessionDomainEvent::TurnStopped { .. }
            | SessionDomainEvent::TurnInterrupted { .. }
            | SessionDomainEvent::SessionFailed { .. } => Turns::apply(&mut state, &event),
            SessionDomainEvent::StepStarted { .. }
            | SessionDomainEvent::StepConcluded { .. }
            | SessionDomainEvent::StepFailed { .. }
            | SessionDomainEvent::StepCancelled { .. }
            | SessionDomainEvent::RunFinished { .. }
            | SessionDomainEvent::RunFailed { .. } => WorkflowRun::apply(&mut state, &event),
            SessionDomainEvent::SubAgentSpawned { .. }
            | SessionDomainEvent::SubAgentRunning { .. }
            | SessionDomainEvent::SubAgentCompleted { .. }
            | SessionDomainEvent::SubAgentFailed { .. }
            | SessionDomainEvent::SubAgentNotified { .. } => SubAgents::apply(&mut state, &event),
            SessionDomainEvent::UsageRecorded { .. } => SessionCore::apply(&mut state, &event),
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
            SessionCommand::Lifecycle(c) => RuntimeLifecycle::handle(self, state, c, ctx).await,
            SessionCommand::Turn(c) => Turns::handle(self, state, c, ctx).await,
            SessionCommand::Run(c) => WorkflowRun::handle(self, state, c, ctx).await,
            SessionCommand::SubAgent(c) => SubAgents::handle(self, state, c, ctx).await,
            SessionCommand::Read(c) => Reads::handle(self, state, c, ctx).await,
            SessionCommand::Hooks(c) => HookRouting::handle(self, state, c, ctx).await,
            SessionCommand::Core(c) => SessionCore::handle(self, state, c, ctx).await,
            // The one command routed by identity rather than by variant: which
            // agent sent the outcome decides which component answers it.
            SessionCommand::AgentOutcome(outcome) => {
                self.on_agent_outcome(state, outcome, ctx).await
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
            // next step a boundary picks.
            self.agents = Some(SessionAgents::workflow());
        } else {
            self.spawn_main_agent(ctx);
        }
        // Each component repairs itself. A self-send rather than direct work,
        // because recovery must not persist and this runs before the first live
        // command — so anything that needs to journal arrives as an ordinary
        // command, down the same path a live one would take.
        let cx = component::ActionCx {
            id: self.id,
            spec: &self.spec,
        };
        let repairs: Vec<SessionCommand> = [
            RuntimeLifecycle::on_load(&cx, state),
            SubAgents::on_load(&cx, state),
            WorkflowRun::on_load(&cx, state),
            Turns::on_load(&cx, state),
        ]
        .into_iter()
        .flatten()
        .collect();
        let repairing = !repairs.is_empty();
        for cmd in repairs {
            let _ = ctx.self_ref().tell(cmd).await;
        }
        // Loading is not a transition, but it is the first moment anyone can
        // learn this status: the supervisor's cache is empty until a session
        // reports, and a page already watching hears nothing otherwise.
        //
        // Skipped when something is being repaired — that command reports the
        // status it lands on, and announcing the pre-repair one first would
        // show a state the session is already leaving.
        if !repairing {
            self.report(state.status.clone()).await;
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
mod tests;
