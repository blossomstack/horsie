//! The vocabulary a session is described in: what it is asked to do, what it
//! records having done, and what it knows as a result.
//!
//! Split out from the actor for one reason — every component names these, and
//! none of them names the actor. Keeping the commands, the events and the folded
//! state together also puts the three halves of the event-sourcing contract in
//! one place: a command decides, an event records, the state is their fold.
//!
//! Nothing here has behaviour beyond `Display`. The decisions live in the
//! components, the fold lives in [`SessionActor::apply_event`](super::SessionActor::apply_event),
//! and this file stays readable as a description of the domain.

use crate::sessions::{
    UserMessageError,
    spec::SessionStatus,
    subagents::{SubAgentForest, SubAgentParent, TreeOwner},
    workflow::WorkflowRunState,
};
use horsie_models::hooks::HookRecord;
use horsie_workflow::{AgentOutcome, AgentUsageSnapshot, UsageTotal};
/// Answering belongs to the agent that asked, so its vocabulary lives with the
/// agent. Re-exported because the session routes both and every caller reaches
/// them through it.
pub use horsie_workflow::{AnswerError, AskAnswer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::oneshot;
use uuid::Uuid;

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
    /// A message for one of this session's agents. Always accepted: the agent
    /// queues it durably and answers it at its next turn, so there is no
    /// rejection path and no `409`.
    ///
    /// The session's part is only to resolve `agent_id` — spawning a cold agent
    /// if need be — and to title an unnamed session from its first message. The
    /// message itself never touches session state: it is addressed to an agent,
    /// and that is where it is stored.
    UserMessage {
        agent_id: Option<String>,
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    },
    /// Cancel the turn in flight. Queued messages are *not* discarded — stop
    /// means "not this turn", not "throw away what I asked for".
    Stop { reply: oneshot::Sender<()> },
    /// Answer every question one agent is parked on, at once. Routed, not
    /// decided: the agent owns what it asked and validates the set.
    Answer {
        agent_id: Option<String>,
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
    /// The main agent started a turn.
    ///
    /// Recorded, not decided: the agent owns its own queue and chooses when
    /// that queue becomes a turn, so this is the session learning what
    /// happened. What the turn consumed and answered is the agent's own fact
    /// and lives in the agent's journal — this carries only the fact that the
    /// session is now `Running`.
    TurnBegan {
        at_ms: u64,
    },
    /// The main agent parked on questions for the user.
    ///
    /// Also recorded rather than decided, and carries no payload for the same
    /// reason: the questions belong to the agent that asked them, which is what
    /// answers them. All this drives is the session's status.
    AskRecorded {
        at_ms: u64,
    },
    TurnEnded {
        at_ms: u64,
    },
    TurnFailed {
        at_ms: u64,
        error: String,
    },
    /// The user cancelled the turn. Distinct from `TurnEnded` only in intent.
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

/// How a turn ended.
///
/// [`AgentOutcome`] minus the two variants that are not a way a turn ends at
/// all: `UsageRecorded`, banked identically for every agent a session hosts,
/// and `Started`, which reports a turn *beginning*. `on_agent_outcome` answers
/// both once before routing. Narrowing them away here is what lets the three
/// components that handle an outcome match exhaustively on the four real cases,
/// instead of each carrying an `unreachable!` for a variant it can never be
/// handed.
///
/// It is a second vocabulary for something `horsie_workflow` already names, and
/// that is the deliberate cost. `AgentOutcome` is the *protocol* between an
/// agent and whatever owns it, and horsie has owners that are not sessions; a
/// session's components want the smaller thing. [`TurnEnd::split`] is the only
/// conversion, and its match is exhaustive, so a variant added to `AgentOutcome`
/// fails to compile there — which is the right place to decide whether it is a
/// way a turn ends or another thing to bank.
pub(super) enum TurnEnd {
    /// The agent produced its output — structured, or its final text.
    Concluded { output: Value },
    /// The agent parked on one or more questions for the user.
    ///
    /// Carries none of them: the questions belong to the agent that asked and
    /// are answered through it, so all this tells the session is that it is now
    /// `AwaitingInput`.
    Asked,
    /// `terminal` means the agent's sandbox is gone and no later message can
    /// bring it back; anything else is a turn the user can retry.
    Failed { error: String, terminal: bool },
    /// The agent parked awaiting its timers, which sessions do not support.
    Parked,
}

impl TurnEnd {
    /// Separate the two things an outcome can be: a turn that ended, or usage to
    /// bank. Both carry the agent that reported them.
    ///
    /// A `Result` rather than an `Option` so the caller cannot reach the routing
    /// path with a non-ending outcome still in hand — the narrowing is total,
    /// and nothing below it needs a case for a variant that never arrives.
    pub(super) fn split(outcome: AgentOutcome) -> Result<(Uuid, Self), (Uuid, NotAnEnd)> {
        match outcome {
            AgentOutcome::Concluded { session_id, output } => {
                Ok((session_id, Self::Concluded { output }))
            }
            AgentOutcome::Asked { session_id, .. } => Ok((session_id, Self::Asked)),
            AgentOutcome::Parked { session_id } => Ok((session_id, Self::Parked)),
            AgentOutcome::Failed {
                session_id,
                error,
                terminal,
                ..
            } => Ok((session_id, Self::Failed { error, terminal })),
            AgentOutcome::UsageRecorded {
                session_id,
                usage_total,
            } => Err((session_id, NotAnEnd::Usage(usage_total))),
            AgentOutcome::Started { session_id } => Err((session_id, NotAnEnd::Started)),
        }
    }
}

/// An outcome that is not a way a turn ended.
pub(super) enum NotAnEnd {
    /// A turn began. The agent decided it, so the session is being told.
    Started,
    /// Tokens to bank. The turn they were spent on is a separate report.
    Usage(UsageTotal),
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
    pub status: SessionStatus,
}

/// A session's aggregated usage.
#[derive(Debug, Clone)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    pub main_agent: AgentUsageEntry,
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
