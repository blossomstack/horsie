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

use crate::agent_loop::{AgentOutcome, AgentUsageSnapshot, UsageTotal};
/// Answering belongs to the agent that asked, so its vocabulary lives with the
/// agent. Re-exported because the session routes both and every caller reaches
/// them through it.
pub use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::{
    UserMessageError,
    spec::{SessionSpec, SessionStatus},
    subagents::{SubAgentForest, SubAgentParent, TreeOwner},
    workflow::WorkflowRunState,
};
use horsie_actor::ReplyTo;
use horsie_models::hooks::HookRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

/// Commands accepted by a [`SessionActor`].
#[derive(Serialize, Deserialize)]
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
#[derive(Serialize, Deserialize)]
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
    /// Internal: the detached create has word of the runtime it asked for —
    /// "the machine is booting" — before it has an outcome. The vendor's own
    /// sentence, carried unedited, because it is what the user is shown.
    NarrateProvisioning { detail: String },
    /// Internal: the detached create finished. Carries the vendor's own error
    /// rather than a summary, because that string is what the user is shown.
    FinishProvisioning {
        error: Option<String>,
        terminal: bool,
    },
    /// The supervisor wants to unload this session. Answers `false` if a run
    /// started in the meantime, in which case nothing has changed and the idle
    /// clock simply restarts.
    PrepareOffload { reply: ReplyTo<bool> },
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: ReplyTo<()> },
}

/// The conversation.
#[derive(Serialize, Deserialize)]
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
        reply: ReplyTo<Result<String, UserMessageError>>,
    },
    /// Cancel the turn in flight. Queued messages are *not* discarded — stop
    /// means "not this turn", not "throw away what I asked for".
    Stop { reply: ReplyTo<()> },
    /// Answer every question one agent is parked on, at once. Routed, not
    /// decided: the agent owns what it asked and validates the set.
    Answer {
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
    },
}

/// The workflow graph.
#[derive(Serialize, Deserialize)]
pub enum RunCommand {
    /// Let the orchestrator start whatever it wants started. Sent to a run at
    /// load so a pending one begins, and after a retry.
    Advance,
    /// Re-run one execution from the run log.
    RetryStep {
        index: u32,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Read this session's workflow run, if it is one.
    State {
        reply: ReplyTo<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Recovery found a step the process died inside. Suspends the run, which is
    /// the state a retry can move.
    ReconcileInterrupted,
}

/// The tree of delegated work.
#[derive(Serialize, Deserialize)]
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
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the spawn's `SubAgentSpawned` write came back — only now
    /// does the child actor exist (persist-then-spawn). A failed write spawns
    /// nothing and the tool gets the error.
    FinishSpawn {
        id: Uuid,
        task: String,
        agent_type: Option<String>,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// The `subagent_status` tool: one node, or the caller's whole subtree.
    Status {
        caller: SubAgentParent,
        id: Option<Uuid>,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under (tree nodes still `Running`). Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    Reconcile,
}

/// Questions answered from the resident actor's memory. None of these touches
/// the journal, so opening a session to look at it costs no sandbox.
#[derive(Serialize, Deserialize)]
pub enum ReadCommand {
    /// Read forward from a cursor in one of the session's agents: `agent_id`
    /// absent or `"main"` for the primary agent, otherwise a subagent id.
    /// `None` answers "no such agent".
    ReadLog {
        agent_id: Option<String>,
        after: Option<crate::agent_loop::Cursor>,
        reply: ReplyTo<Option<crate::agent_loop::ReadOutcome>>,
    },
    /// Read a window *backwards* from a cursor — scroll-back.
    PageLog {
        agent_id: Option<String>,
        before: Option<u64>,
        max: usize,
        reply: ReplyTo<Option<crate::agent_loop::LogPage>>,
    },
    /// Read one agent's document: what it is, what became of it, and its live
    /// values. `agent_id` absent or `"main"` for the primary agent — which, on
    /// a run, is the step in flight. `None` answers "no such agent".
    Agent {
        agent_id: Option<String>,
        reply: ReplyTo<Option<AgentDetail>>,
    },
    /// Read this session's recovered state: status, usage, and its agents.
    Snapshot { reply: ReplyTo<SessionSnapshot> },
    /// Read this session's aggregated usage.
    UsageStats { reply: ReplyTo<SessionUsageStats> },
}

/// What plugin hooks did. Pure routing: nothing here is persisted by the
/// session, and nothing here changes state.
#[derive(Serialize, Deserialize)]
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
#[derive(Serialize, Deserialize)]
pub enum CoreCommand {
    /// Set the session title from the built-in title tool.
    SetTitle {
        title: String,
        reply: ReplyTo<Result<String, String>>,
    },
    /// A rename that happened elsewhere — a person renaming from the list, or
    /// the supervisor telling a resident session what it just recorded.
    ///
    /// Journals it here too. The supervisor's copy is what the session list
    /// shows; this one is what the running session reads, and a session's own
    /// journal is the truth about that session.
    TitleSet { name: String },
    /// Internal: write this session's spec into its own log.
    ///
    /// Self-sent by recovery when the log has no spec, which is true exactly
    /// twice — for a session being created, and for one whose process died
    /// between the supervisor recording it and this write. Both take the same
    /// path, so the crash case is not a special case.
    RecordSpec {
        spec: Box<crate::sessions::spec::SessionSpec>,
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
    /// What this session is. The first thing a session journals, and the only
    /// thing a host needs besides the id to run it.
    ///
    /// Boxed because a spec is much larger than any other variant here, and an
    /// enum is as big as its widest arm.
    SpecRecorded {
        spec: Box<SessionSpec>,
    },
    /// This session was given a name — by a person, by the title tool, or
    /// derived from the first message.
    ///
    /// Journaled here as well as in the supervisor's list because this is the
    /// copy the running session reads. The supervisor's is what the session
    /// list shows, and it is told separately.
    Renamed {
        name: String,
    },
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
    /// What the vendor said about a create still in flight, in its own words.
    ///
    /// Narration, and the only variant here that decides nothing: the status is
    /// settled by the three facts around it. It exists because a create is a
    /// wait a person sits through, and "provisioning" on its own does not say
    /// whether a machine is booting, resuming, or has been queued behind a
    /// substrate that is out of capacity.
    ProvisioningProgress {
        at_ms: u64,
        detail: String,
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
/// components that handle an outcome match exhaustively on the five real cases,
/// instead of each carrying an `unreachable!` for a variant it can never be
/// handed.
///
/// It is a second vocabulary for something `crate::agent_loop` already names, and
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
    /// The process died inside the turn, and the agent said so at recovery.
    ///
    /// The one end that produces nothing — no output, no questions, no error to
    /// show. Only the main agent's is acted on: a subagent's node and a step's
    /// log entry are repaired from state the *session* owns, at session load,
    /// and those agents stay cold long enough that their own report would
    /// arrive after the repair rather than instead of it.
    Interrupted,
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
            AgentOutcome::Concluded { agent, output } => Ok((agent, Self::Concluded { output })),
            AgentOutcome::Asked { agent, .. } => Ok((agent, Self::Asked)),
            AgentOutcome::Parked { agent } => Ok((agent, Self::Parked)),
            AgentOutcome::Interrupted { agent } => Ok((agent, Self::Interrupted)),
            AgentOutcome::Failed {
                agent,
                error,
                terminal,
                ..
            } => Ok((agent, Self::Failed { error, terminal })),
            AgentOutcome::UsageRecorded { agent, usage_total } => {
                Err((agent, NotAnEnd::Usage(usage_total)))
            }
            AgentOutcome::Started { agent } => Err((agent, NotAnEnd::Started)),
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
    /// What this session *is* — vendor, agent settings, workflow, name.
    ///
    /// In the session's own journal, not just the supervisor's, because a host
    /// that never saw the request creating this session has no parent to take
    /// it from: it recovers the log and the spec is in it. The supervisor keeps
    /// its own copy for the session list, which is a seed and an index rather
    /// than the truth — a session's journal is the truth about that session.
    ///
    /// `None` means the spec has not been recorded yet, which happens for
    /// exactly as long as it takes a newly created session to journal it, and
    /// after a process died in that window. Such a session refuses work and
    /// asks its supervisor for the seed rather than guessing.
    #[serde(default)]
    pub spec: Option<SessionSpec>,
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
    /// Tokens banked across every agent this session hosts. Banked, so a turn
    /// in flight is not in it and nothing has to be asked of an agent.
    pub fn session_usage_total(&self) -> UsageTotal {
        self.agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u))
    }

    pub fn root_owner(&self) -> TreeOwner {
        match self.run.as_ref().and_then(WorkflowRunState::current_agent) {
            Some(agent) => TreeOwner::Step(agent),
            None => TreeOwner::Main,
        }
    }
}

/// One agent's own usage/context-size snapshot, labeled with the model it ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUsageEntry {
    pub model: String,
    pub snapshot: AgentUsageSnapshot,
}

/// What a reader needs to know about a session, answered by the actor that owns
/// it. Every field is recovered from the journal, so an unloaded session gives
/// the same answers as a loaded one — it just has to be loaded to give them.
///
/// The whole live half of `GET /api/sessions/:id`, so that document is one ask.
/// It used to be four — status here, usage, the subagent tree and the run log
/// each separately — all four served by this same actor, and reassembled above
/// it by an HTTP handler that had to know what kind of session this was to do
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub status: SessionStatus,
    /// Tokens summed across every agent this session hosts. The per-agent
    /// breakdown is [`SessionUsageStats`], which only the run graph needs.
    pub usage_total: UsageTotal,
    /// Every agent this session hosts, in the vocabulary of
    /// `/sessions/:id/agents/:agent_id`.
    pub agents: Vec<AgentEntry>,
}

/// What became of one of a session's agents.
///
/// One vocabulary for three different underlying facts — a conversation's main
/// agent takes its state from the session, a run's step agent from the run log,
/// a subagent from the tree — because to a reader they are one question. Asked
/// three times above the actor, they became three projections that disagreed: a
/// concluded step answered `running` for ever, and a session whose runtime
/// never built answered `idle` beside a status that said `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// The session's runtime is still being built. Nothing has run yet.
    Provisioning,
    Running,
    /// Loaded and not working — where a conversation's agent rests between
    /// turns, and the one state that is not an ending.
    Idle,
    /// Parked on a question, waiting for an answer.
    AwaitingInput,
    /// Ran to a result. Only a subagent or a step reaches it: a conversation is
    /// never *done*.
    Completed,
    Failed,
    Cancelled,
}

/// One agent a session hosts: which agent it is, what became of it, and when.
///
/// What it *said* is not here — a transcript is read from the agent's own log,
/// through `/history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// `"main"`, or the agent's uuid. The vocabulary every agent-scoped route
    /// speaks.
    pub id: String,
    /// The agent that spawned this one. Absent for a main agent, for a step —
    /// which the definition chose, not an agent — and for a subagent rooted
    /// directly on either.
    pub parent: Option<Uuid>,
    /// A subagent's label, or the name of the step this agent is one execution
    /// of. Absent for a main agent, which is not one of several.
    pub label: Option<String>,
    pub depth: u32,
    /// The plugin-declared agent type a typed subagent runs as.
    pub agent_type: Option<String>,
    pub status: AgentStatus,
    pub error: Option<String>,
    /// When this agent started and when it reached its result. Zero for a main
    /// agent — nothing spawned it, and it is as old as the session, whose
    /// `created_at` is on the same document — and zero for `ended_at_ms` while
    /// an agent is still running.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

/// Everything a session knows about one of its agents: its entry in the roster,
/// what it ran under, what it produced, and its live values.
///
/// One answer rather than a tree read, a run read and a state read stitched
/// together by the caller — which is what left a step's document reporting the
/// session's model and a permanent `running`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetail {
    pub entry: AgentEntry,
    /// The model this agent runs under: a step's own preset, or the session's.
    pub model: String,
    /// What a subagent was asked to do. A main agent is asked things one turn
    /// at a time, and a step's brief is its definition's.
    pub task: Option<String>,
    /// Its terminal result, once it has one. A step's structured output is
    /// rendered the same way a subagent's report is, because a reader wants the
    /// same thing from both.
    pub output: Option<String>,
    /// Read from the agent itself: its task list, its usage, and where in its
    /// log those were taken.
    pub state: crate::agent_loop::AgentStateView,
}

/// A session's aggregated usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    pub main_agent: AgentUsageEntry,
    /// Every agent's banked total, keyed as `agent_usage` keys it: `"main"` for
    /// the primary agent, the agent's uuid for a subagent or a workflow step.
    ///
    /// Here so a run can report per-step tokens: a step's key *is* its
    /// `StepRun.agent`, so the run graph only needs the map, not a read per
    /// step. Usage banks at turn end, so a step in flight reads zero — the same
    /// as `session_total`.
    pub agents: HashMap<String, UsageTotal>,
}

/// Which agent of a session a broadcast belongs to. `Main` is not a `Uuid`
/// variant because the main agent's journal is keyed by the *session* id — the
/// two namespaces are deliberately distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentKey {
    Main,
    Sub(Uuid),
    /// One execution of a workflow step. Its own key, not `Sub`: a step is not
    /// spawned by an agent, it is chosen by the definition, and it roots a
    /// subagent tree of its own.
    Step(Uuid),
}
