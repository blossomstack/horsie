//! The vocabulary a session is described in: what it is asked to do, what it
//! records having done, and what it knows as a result.
//!
//! Split out from the actor for one reason — every component names these, and
//! none of them names the actor. Keeping the commands, the events and the
//! folded state together also puts the three halves of the event-sourcing
//! contract in one place: a command decides, an event records, the state is
//! their fold.
//!
//! Nothing here has behaviour beyond `Display`. The decisions live in the
//! components, the fold lives in
//! [`SessionActor::apply_event`](super::SessionActor::apply_event), and this
//! file stays readable as a description of the domain.

use crate::agent_loop::{AgentOutcome, AgentUsageSnapshot, UsageTotal};
/// Answering belongs to the agent that asked, so its vocabulary lives with the
/// agent. Re-exported because the session routes both and every caller reaches
/// them through it.
pub use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::{
    UserMessageError,
    run_forest::RuntimeChoice,
    run_forest::{RunForest, RunId, RunState, SeedMode, TurnPhase},
    spec::{AgentSettings, RuntimeId, SessionSpec, SessionStatus},
    workflow::{WorkflowRunSpec, WorkflowRunStatus},
};
use horsie_actor::ReplyTo;
use horsie_models::hooks::HookRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use uuid::Uuid;

/// Commands accepted by a [`SessionActor`].
#[derive(Serialize, Deserialize)]
pub enum SessionCommand {
    /// Getting and releasing this session's sandbox.
    Lifecycle(LifecycleCommand),
    /// The session: what a person sends and how a turn ends.
    Turn(TurnCommand),
    /// The workflow graph, when this session is a run.
    Run(RunCommand),
    /// The tree of delegated work.
    SubAgent(SubAgentCommand),
    /// Branching a session into a second one inside this session.
    SubSession(SubSessionCommand),
    /// Questions answered without waking anything.
    Read(ReadCommand),
    /// What plugin hooks did, routed to the agent it happened to.
    Hooks(HookCommand),
    /// The session's own bookkeeping: its title, and preparation progress.
    Core(CoreCommand),
    /// Internal: an agent reported its terminal outcome. Top-level because it
    /// is the one command routed by *identity* rather than by variant — which
    /// agent sent it decides which component answers.
    AgentOutcome(AgentOutcome),
}

/// Getting and releasing this session's sandbox.
#[derive(Serialize, Deserialize)]
pub enum LifecycleCommand {
    /// Build a runtime for `owner`, from `env`.
    ///
    /// Sent by the supervisor as part of creating the session, by the session
    /// itself when it loads to find a create the process died inside, and by a
    /// sub session that asked for an environment of its own. Idempotent against
    /// a runtime already built for that owner: the record's own state is what
    /// keeps "provisioned exactly once" true, with no bookkeeping beyond the
    /// journal.
    ///
    /// `env` is `None` for the re-attempt paths, which re-use the environment
    /// the record already carries — a retry must build the same sandbox, not
    /// re-resolve one that may since have been edited.
    Provision {
        owner: Uuid,
        env: Option<Box<crate::sessions::spec::RuntimeEnv>>,
    },
    /// Internal: the detached create has word of the runtime it asked for —
    /// "the machine is booting" — before it has an outcome. The vendor's own
    /// sentence, carried unedited, because it is what the user is shown.
    NarrateProvisioning { runtime: RuntimeId, detail: String },
    /// Internal: the detached create finished. Carries the vendor's own error
    /// rather than a summary, because that string is what the user is shown.
    FinishProvisioning {
        runtime: RuntimeId,
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

/// The session.
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
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
    },
    /// Cancel one agent's turn in flight. Queued messages are *not* discarded —
    /// stop means "not this turn", not "throw away what I asked for".
    ///
    /// Addressed, never session-wide: a session hosts several sessions at
    /// once and each has its own turn, so "stop the session" named no single
    /// thing to cancel. `agent_id` is `"main"` or an agent's uuid, the same
    /// vocabulary every other agent-scoped request speaks.
    ///
    /// `Err` is for an id that names no agent here. An agent that is simply not
    /// working is `Ok`: nothing to stop is not a failure, and a client racing a
    /// turn's own end would otherwise see an error for winning the race.
    Stop {
        agent_id: String,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Answer every question one agent is parked on, at once. Routed, not
    /// decided: the agent owns what it asked and validates the set.
    Answer {
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
    },
}

/// The workflow runs this session hosts: its own, and the ones its agents
/// invoke mid-session.
#[derive(Serialize, Deserialize)]
pub enum RunCommand {
    /// The `invoke_workflow` tool: start a run of `graph` under `parent`, the
    /// agent that called it. The graph arrives already resolved — the tool
    /// resolves presets on its own task, off this mailbox — so the session
    /// only checks its limits and journals the snapshot.
    Create {
        parent: Uuid,
        graph: Arc<WorkflowRunSpec>,
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the create's `RunCreated` write came back — only now is the
    /// run real (persist-then-reply, the same shape a subagent spawn has). A
    /// failed write starts nothing and the tool gets the error.
    FinishCreate {
        id: Uuid,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// The `workflow_status` tool: one run's phase and step log, visible to
    /// the agent that invoked it and to its ancestors.
    Status {
        caller: Uuid,
        run: Uuid,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Let the orchestrators start whatever they want started. Sent at load so
    /// a pending run begins, and after a retry.
    Advance,
    /// Re-run one execution from the root run's log.
    RetryStep {
        index: u32,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Read this session's own workflow run, if it is one.
    State {
        reply: ReplyTo<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Recovery found steps the process died inside. Suspends their runs,
    /// which is the state a retry can move.
    ReconcileInterrupted,
}

/// The tree of delegated work.
#[derive(Serialize, Deserialize)]
pub enum SubAgentCommand {
    /// The `spawn_agent` tool: start a subagent under `caller` — the agent
    /// that called it, whichever kind it is.
    Spawn {
        caller: Uuid,
        title: String,
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
        caller: Uuid,
        id: Option<Uuid>,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under (tree nodes still `Running`). Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    Reconcile,
}

/// What accepting a message produced.
///
/// More than the message's id because one message can do more than queue
/// itself: `/fork` creates a session, and the client has to be told which
/// one to open. A field rather than a second endpoint, so every client that can
/// send a message can sub session without learning a new call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAccepted {
    pub message_id: String,
    /// The sub session this message created. Absent for every ordinary
    /// message, which is what makes the field additive.
    pub sub_session: Option<String>,
}

impl MessageAccepted {
    /// An ordinary message, which created no sub session.
    #[must_use]
    pub fn queued(message_id: String) -> Self {
        Self {
            message_id,
            sub_session: None,
        }
    }
}

/// Branching a session into a second one inside this session.
///
/// A sub session is a session, not delegated work: nothing here reports a
/// result to anybody, and the only reply any of it carries is the sub
/// session's own id, which is what a client redirects to.
#[derive(Serialize, Deserialize)]
pub enum SubSessionCommand {
    /// `/fork` or `/summary-n-fork`: branch the session of agent `parent` —
    /// the main agent or another sub session — and queue `message` in the new
    /// sub session so it has something to do when its seed lands.
    Create {
        parent: Uuid,
        seed: SeedMode,
        message: String,
        /// What to call it. A sub session has no `set_session_title` of its
        /// own — it is named by whoever branched it, at the moment it is
        /// branched, so it is never a session with no name.
        title: String,
        /// Where the sub session should run, already resolved.
        env: RequestedRuntime,
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the `ForkCreated` write came back — only now does the sub
    /// session's actor exist (persist-then-spawn, exactly as a subagent spawn
    /// does). A failed write spawns nothing and the caller gets the error.
    FinishCreate {
        id: Uuid,
        /// Carried through from `Create` rather than parked in a map: the
        /// environment belongs to this one create, and a map keyed by id would
        /// be a second thing to keep in step with the journal.
        env: RequestedRuntime,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// Internal: the detached seeding task wrote the sub session's initial
    /// state, so the sub session may run and the message waiting in its queue
    /// is released.
    Seeded { id: Uuid },
    /// Internal: the source agent's `/summary-n-fork` turn produced the summary
    /// these sub sessions were waiting on.
    ///
    /// A list because sub sessions queued into one turn share a branch point,
    /// so one provider call serves all of them.
    Summarised {
        sub_sessions: Vec<Uuid>,
        result: Result<String, String>,
    },
    /// Internal: the detached seeding task could not. Carries the reason
    /// verbatim, because that string is what the user is shown.
    SeedFailed { id: Uuid, error: String },
    /// Internal: recovery found sub sessions a dead process abandoned mid-seed.
    ReseedInterrupted,
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
    /// Read a bounded, filtered window of one agent's log.
    PageLog {
        agent_id: Option<String>,
        anchor: crate::agent_loop::Anchor,
        max: usize,
        filter: crate::agent_loop::LogFilter,
        reply: ReplyTo<Option<crate::agent_loop::LogPage>>,
    },
    /// Find where in one agent's log something was said.
    SearchLog {
        agent_id: Option<String>,
        needle: String,
        max: usize,
        filter: crate::agent_loop::LogFilter,
        reply: ReplyTo<Option<Vec<horsie_models::session_api::LogSearchHit>>>,
    },
    /// Resolve an entry id to its seq within one agent's log.
    SeqOfId {
        agent_id: Option<String>,
        entry_id: String,
        reply: ReplyTo<Option<Option<u64>>>,
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

/// The first thing said to a session, carried by the command that creates it.
///
/// Travels *with* the spec rather than behind it: a message is the one thing a
/// create cannot lose without a caller noticing, and a second addressed send is
/// exactly what could lose it. Answered by the session once the write is
/// durable — the same promise [`TurnCommand::UserMessage`] makes.
#[derive(Serialize, Deserialize)]
pub struct FirstMessage {
    pub text: String,
    pub reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
}

/// The session's own bookkeeping.
#[derive(Serialize, Deserialize)]
pub enum CoreCommand {
    /// Remove one agent this session hosts, and everything below it.
    ///
    /// Here rather than on either agent-owning component because the id does
    /// not say which kind it is: resolving that is the session's job, and the
    /// removal is identical once it has.
    DeleteAgent {
        id: Uuid,
        reply: ReplyTo<Result<(), String>>,
    },
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
    /// Bring this session into being: what it is, and the first thing said to
    /// it, in one message.
    ///
    /// **One command and not three, and that is the whole point.** This used to
    /// be a `RecordSpec` the supervisor followed with a `Provision` and a
    /// `UserMessage`. Each was addressed through the session shard separately,
    /// and placement is resolved once per send — so the command that
    /// *materialised* the actor was not guaranteed to be the one carrying the
    /// spec. When it was not, the session recovered an empty log, found no
    /// spec, and the first handler to read one took the actor down with it.
    /// `POST /sessions` reported that as a 500, a 404 or a 409 depending on
    /// which half of the create lost the race.
    ///
    /// Everything this fans out to is a *self*-send, which is in-process and
    /// ordered behind this command, so nothing below here can be reordered or
    /// placed on another node.
    ///
    /// Idempotent against a log that already has a spec, which is what makes a
    /// redelivery — and a process that died between the supervisor's record and
    /// this write — the same path rather than a special case.
    Create {
        spec: Box<crate::sessions::spec::SessionSpec>,
        /// What to call it, when the creator said. Journaled as the main
        /// agent's title, because the main agent is the session and its title
        /// is the session's name.
        name: Option<String>,
        /// The first thing said to this session, when it was created with one.
        message: Option<FirstMessage>,
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
    ///
    /// Carries the session's own id: the fold roots the run forest from this
    /// event, main's entry is keyed by the session id, and a pure fold has no
    /// other way to learn it.
    SpecRecorded {
        at_ms: u64,
        session: Uuid,
        spec: Box<SessionSpec>,
    },
    /// The main agent was given a title — by a person, by `set_session_title`,
    /// or derived from the first message. The session's name *is* that title:
    /// the main agent is the session, so there is one fact and one owner.
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
        /// Which runtime is being built. A session may own several — its own,
        /// and one per sub session that asked for an environment of its own —
        /// so every provisioning fact has to name the one it is about.
        runtime: RuntimeId,
    },
    /// A session or sub session asked for a runtime of its own.
    ///
    /// Separate from [`Self::ProvisioningStarted`] because a create may be
    /// re-attempted and the environment may not: this carries the snapshot
    /// once, and the retries that follow carry only the attempt. It is also
    /// what points the asking conversation at the runtime, which is how every
    /// agent below it resolves one.
    RuntimeRequested {
        at_ms: u64,
        runtime: RuntimeId,
        /// The session or sub session that asked. Its entry gets the runtime,
        /// and deleting it is what shuts the runtime down.
        owner: Uuid,
        /// What to build it from, resolved at the moment it was asked for.
        env: crate::sessions::spec::RuntimeEnv,
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
        runtime: RuntimeId,
        detail: String,
    },
    /// The vendor confirmed the runtime. The session becomes ordinary here,
    /// and whatever queued behind the create starts.
    ProvisioningSucceeded {
        at_ms: u64,
        runtime: RuntimeId,
    },
    /// The create failed. `terminal` carries the one distinction that matters:
    /// a live vendor refusing to produce the runtime ends the session, while an
    /// offline vendor or a failed token mint leaves it retryable.
    ProvisioningFailed {
        at_ms: u64,
        runtime: RuntimeId,
        error: String,
        terminal: bool,
    },
    /// The main agent started a turn.
    ///
    /// Recorded, not decided: the agent owns its own queue and chooses when
    /// that queue becomes a turn, so this is the session learning what
    /// happened. What the turn consumed and answered is the agent's own fact
    /// and lives in the agent's journal — this carries only the fact that the
    /// session is now running.
    TurnBegan {
        at_ms: u64,
        agent: Uuid,
    },
    /// `agent` parked on questions for the user — the main agent, or a
    /// workflow step, whose park holds its run.
    ///
    /// Also recorded rather than decided, and carries no payload for the same
    /// reason: the questions belong to the agent that asked them, which is what
    /// answers them. All this drives is the owning entry's phase.
    AskRecorded {
        at_ms: u64,
        agent: Uuid,
    },
    TurnEnded {
        at_ms: u64,
        agent: Uuid,
    },
    TurnFailed {
        at_ms: u64,
        agent: Uuid,
        error: String,
    },
    /// The user cancelled the turn. Distinct from `TurnEnded` only in intent.
    TurnStopped {
        at_ms: u64,
        agent: Uuid,
    },
    /// Recovery found a turn that the process died in. Recorded rather than
    /// inferred, so the transition is in the log like every other one.
    TurnInterrupted {
        at_ms: u64,
        agent: Uuid,
    },
    /// Terminal: this session can never run again.
    SessionFailed {
        at_ms: u64,
        reason: String,
    },
    /// A subagent or a sub session was removed, because someone asked. Never
    /// automatic, and never the main agent or a workflow step: the first *is*
    /// the session and the second belongs to its run's log.
    ///
    /// One event for both kinds, and `SessionCore`'s rather than either
    /// component's, for the reason `UsageRecorded` is: two components own
    /// agents, the removal is identical for both, and the fold takes the whole
    /// subtree — which can span both kinds anyway.
    AgentDeleted {
        at_ms: u64,
        id: Uuid,
    },
    /// One agent's cumulative usage after a completed run, and how full its
    /// context was when the run ended. Durable here so neither the
    /// session-level total nor a per-agent readout requires waking an idle
    /// agent — which is the whole point: a graph of thirty agents must not be
    /// thirty recoveries.
    UsageRecorded {
        at_ms: u64,
        agent_id: String,
        usage_total: UsageTotal,
        /// Tokens the agent's context held at that point. Zero before the
        /// agent has run a turn.
        context_tokens: u32,
    },
    /// A subagent was spawned by agent `parent` — main, a step, a sub session,
    /// or another subagent. Persisted before the child actor exists — a crash
    /// between the two replays as a node that recovery reconciles to failed.
    ///
    /// The parent is the actual agent, resolved when the tool was called;
    /// depth is a walk over the forest, so neither is re-derived at fold time.
    SubAgentSpawned {
        at_ms: u64,
        id: Uuid,
        parent: Uuid,
        title: String,
        task: String,
        /// The plugin-declared agent type this subagent runs as, if any.
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
    /// An agent invoked a workflow mid-session. Persisted before anything
    /// starts, with the graph snapshotted onto the event — replay never
    /// reaches a store, and a crash between this write and the tool's ack
    /// replays as a pending run the next boundary simply starts.
    RunCreated {
        at_ms: u64,
        id: Uuid,
        /// The agent the run reports to.
        parent: Uuid,
        graph: Arc<WorkflowRunSpec>,
    },
    /// One execution of one workflow step began. Appended, never replacing: a
    /// loop back onto a step and a retry of one are both new entries, which is
    /// what keeps the log replayable and the graph projection lossless.
    StepStarted {
        at_ms: u64,
        /// The run this execution belongs to — the session's own, or invoked.
        run: Uuid,
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
        run: Uuid,
        index: u32,
        output: Value,
    },
    StepFailed {
        at_ms: u64,
        run: Uuid,
        index: u32,
        error: String,
    },
    /// A step was cancelled — by an interrupt, or by a retry taking its place.
    /// Suspends the run: a person decides between retrying and abandoning,
    /// because the step's effect on the shared workspace is unknown.
    StepCancelled {
        at_ms: u64,
        run: Uuid,
        index: u32,
    },
    RunFinished {
        at_ms: u64,
        run: Uuid,
        output: Value,
    },
    RunFailed {
        at_ms: u64,
        run: Uuid,
        error: String,
    },
    /// An invoked run's terminal result was sent to the agent that invoked it.
    /// Persisted in the same effect as the send, exactly as a subagent's
    /// `SubAgentNotified` is, so a reload neither re- nor never-sends.
    RunNotified {
        at_ms: u64,
        run: Uuid,
    },
    /// A session was branched. Persisted before the sub session's actor exists
    /// — a crash between the two replays as a sub session still
    /// `Provisioning`, which `SubSessions::on_load` re-seeds. Strictly better
    /// than an untracked agent, which is the same trade `SubAgentSpawned`
    /// makes.
    SubSessionCreated {
        at_ms: u64,
        id: Uuid,
        /// The agent whose session was branched: main, or another sub session.
        parent: Uuid,
        /// The source agent's log seq at the branch point.
        source_seq: u64,
        seed: SeedMode,
        /// What the sub session was created to do. On the event so a sub
        /// session abandoned mid-seed can be re-seeded with it, rather than
        /// coming back idle with nothing to answer.
        message: String,
        /// What to call it, chosen by whoever branched it.
        title: String,
        /// Where it runs, as it was asked for.
        ///
        /// The three answers the tool accepts, one variant each: `Inherit` for
        /// an omitted `environment`, `Without` for `"none"`, and `Pending` for
        /// a named one — whose record lands moments later, and until then
        /// nothing may run in it.
        ///
        /// `#[serde(default)]` so sub sessions journaled before a session could
        /// own more than one runtime replay as inheriting, which is what they
        /// did.
        #[serde(default = "crate::sessions::run_forest::inherit")]
        runtime: RuntimeChoice,
    },
    /// The sub session's initial state is durable, so it may run and the
    /// message seeded alongside it is drained.
    SubSessionSeeded {
        at_ms: u64,
        id: Uuid,
    },
    /// A sub session moved. Journaled so the session list can show it without
    /// loading the session, exactly as the session's own status is.
    SubSessionStatusChanged {
        at_ms: u64,
        id: Uuid,
        status: AgentStatus,
    },
    /// One of a sub session's turns ended, however it ended.
    ///
    /// One variant carrying an outcome, where the main agent's turn has four
    /// siblings (`TurnEnded`/`TurnFailed`/`TurnStopped`/`TurnInterrupted`):
    /// those four exist because each moves the *session's* status differently,
    /// and a sub session moves only its own roster entry, which is a function
    /// of the outcome. Deriving the status here is also what stops the two
    /// from disagreeing.
    ///
    /// Separate from `ForkStatusChanged` because it is the sub session's turn
    /// *boundary*, and a boundary is the one thing a reader must see in the
    /// sub session's own transcript — a status is not.
    SubSessionTurnEnded {
        at_ms: u64,
        id: Uuid,
        outcome: horsie_agentcore::TurnOutcome,
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
/// It is a second vocabulary for something `crate::agent_loop` already names,
/// and that is the deliberate cost. `AgentOutcome` is the *protocol* between
/// an agent and whatever owns it, and horsie has owners that are not sessions;
/// a session's components want the smaller thing. [`TurnEnd::split`] is the
/// only conversion, and its match is exhaustive, so a variant added to
/// `AgentOutcome` fails to compile there — which is the right place to decide
/// whether it is a way a turn ends or another thing to bank.
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
    /// Separate the two things an outcome can be: a turn that ended, or usage
    /// to bank. Both carry the agent that reported them.
    ///
    /// A `Result` rather than an `Option` so the caller cannot reach the
    /// routing path with a non-ending outcome still in hand — the narrowing is
    /// total, and nothing below it needs a case for a variant that never
    /// arrives.
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
            AgentOutcome::UsageRecorded {
                agent,
                usage_total,
                context_tokens,
            } => Err((
                agent,
                NotAnEnd::Usage {
                    usage_total,
                    context_tokens,
                },
            )),
            AgentOutcome::Started { agent } => Err((agent, NotAnEnd::Started)),
            AgentOutcome::SeedSummary {
                agent,
                sub_sessions,
                result,
            } => Err((
                agent,
                NotAnEnd::SeedSummary {
                    sub_sessions,
                    result,
                },
            )),
        }
    }
}

/// An outcome that is not a way a turn ended.
pub(super) enum NotAnEnd {
    /// A turn began. The agent decided it, so the session is being told.
    Started,
    /// Tokens to bank, and how full the agent's context is now. The turn they
    /// were spent on is a separate report.
    Usage {
        usage_total: UsageTotal,
        context_tokens: u32,
    },
    /// The summary a `/summary-n-fork` turn was asked for. Nothing about how
    /// that turn ended — it is still running, or it ended some other way.
    SeedSummary {
        sub_sessions: Vec<Uuid>,
        result: Result<String, String>,
    },
}

/// What a caller asked for when it branched a sub session.
///
/// The three answers `spawn_subsession`'s `environment` parameter accepts, as
/// three variants. It used to be an `Option<Option<_>>`, where the outer
/// `None` meant "omitted" and the inner one meant `"none"` — two absences a
/// reader had to hold in their head, on the decision where getting it backwards
/// silently takes a sub session's filesystem away.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RequestedRuntime {
    /// The parameter was omitted: run wherever the parent runs, including
    /// nowhere.
    Inherit,
    /// `"none"`: run with no sandbox.
    Without,
    /// A named environment, already resolved.
    Own(Box<crate::sessions::spec::RuntimeEnv>),
}

/// Where one agent runs, as this state can answer it.
///
/// Three answers, never two. "No runtime" used to mean both "it asked for none"
/// and "nothing here can say yet", and the two demand opposite behaviour: the
/// first is ready to run, the second must wait. Collapsed into one `None`, a
/// session ran its whole first turn believing it had no sandbox.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentRuntime<'a> {
    /// It runs on this one.
    On(RuntimeId, &'a RuntimeRecord),
    /// It asked for no sandbox, and is ready to run without one.
    Without,
    /// It should have one and this state cannot yet say which. Never run here.
    Pending,
}

/// One runtime this session owns, and where its build got to.
///
/// The session holds a map of these rather than one `ProvisioningState`,
/// because a sub session may run on a sandbox of its own: a root idling while
/// one of its branches boots a machine is now an ordinary shape, and a single
/// session-wide provisioning field could not say it.
///
/// `owner` is what decides the runtime's lifetime. A runtime belongs to the
/// session or sub session that *asked* for it — deleting that one shuts it
/// down, and deleting a sub session that merely inherited its parent's changes
/// nothing. One field, no reference counting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    /// What to build it from, snapshotted when it was asked for.
    pub env: crate::sessions::spec::RuntimeEnv,
    /// The agent id of the session or sub session that created it.
    pub owner: Uuid,
    pub provisioning: ProvisioningState,
}

/// The runtime-build half of a session's life, folded from the provisioning
/// events. Its own slice rather than writes into a shared `status` field, so
/// the status can be a projection with one owner per fact.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum ProvisioningState {
    /// No create has ever been asked for.
    #[default]
    Never,
    /// A create is outstanding. Found at load, it means the process died
    /// mid-create — safe to re-attempt, because no turn can have run under it.
    InFlight { at_ms: u64 },
    /// The vendor confirmed the runtime. `at_ms` is the provision's identity:
    /// re-acquiring the sandbox means addressing the one already running, and
    /// a server that forgot which provision that was could not name it.
    Ready { at_ms: u64 },
    /// The create failed on something retryable — an offline vendor, a token
    /// that could not be minted. A terminal refusal is `SessionState::fatal`
    /// instead: it ends the session, and this variant must stay re-attemptable.
    Failed { at_ms: u64, reason: String },
}

impl ProvisioningState {
    /// The identity of the current (or last attempted) provision.
    #[must_use]
    pub fn at_ms(&self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::InFlight { at_ms } | Self::Ready { at_ms } | Self::Failed { at_ms, .. } => {
                Some(*at_ms)
            }
        }
    }
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
    /// Terminal, session-wide failure: the one status a session cannot leave.
    /// Everything else about "how is it going" lives on the entry it is true
    /// of, in the forest.
    pub fatal: Option<String>,
    /// Every runtime this session owns, keyed by its own id.
    ///
    /// A map rather than one `ProvisioningState`, because a sub session may run
    /// on a sandbox of its own: a root idling while one of its branches boots a
    /// machine is an ordinary shape now, and a single session-wide field could
    /// not say it. Which agent runs on which is *not* here — that lives on the
    /// run entries, resolved by [`RunForest::runtime_of_agent`], so there is
    /// one owner of "who runs where" and one of "how is it doing".
    #[serde(default)]
    pub runtimes: BTreeMap<RuntimeId, RuntimeRecord>,
    #[serde(default)]
    pub agent_usage: HashMap<String, UsageTotal>,
    /// How full each agent's context was at the end of its last turn, keyed as
    /// `agent_usage` is. Banked from the same event, so a roster can report it
    /// without waking anything.
    #[serde(default)]
    pub agent_context_tokens: HashMap<String, u32>,
    /// Every unit of work this session hosts — the main session, its
    /// workflow runs, every subagent and every sub session — as one hierarchy.
    #[serde(default)]
    pub forest: RunForest,
}

impl SessionState {
    /// How the runtime `agent` runs on is doing.
    ///
    /// `None` means there is nothing to wait for: either the agent's session
    /// chose no runtime, or it named one this state has never heard of. Both
    /// read as "no sandbox", which is what every caller does about it.
    #[must_use]
    pub fn provisioning_for(&self, agent: Uuid) -> Option<&ProvisioningState> {
        match self.runtime_for(agent) {
            AgentRuntime::On(_, rec) => Some(&rec.provisioning),
            AgentRuntime::Without | AgentRuntime::Pending => None,
        }
    }

    /// The record for the runtime `agent` runs on, with its id.
    #[must_use]
    pub fn runtime_for(&self, agent: Uuid) -> AgentRuntime<'_> {
        self.runtime_of_choice(self.forest.runtime_of_agent(agent))
    }

    /// The record behind a choice the forest already resolved.
    ///
    /// Split out because a step's runtime is resolved by walking from its
    /// *run* rather than from itself, and both walks land on the same lookup.
    pub fn runtime_of_choice(&self, choice: RuntimeChoice) -> AgentRuntime<'_> {
        match choice {
            RuntimeChoice::On(id) => match self.runtimes.get(&id) {
                Some(rec) => AgentRuntime::On(id, rec),
                // Pointed at a runtime this state has never heard of. A broken
                // journal, not a configuration — so it waits rather than
                // running without.
                None => AgentRuntime::Pending,
            },
            RuntimeChoice::Without => AgentRuntime::Without,
            // Nothing here can say yet. `Inherit` lands here only when the
            // walk ran off the top of the forest, which is the same amount of
            // knowledge as no walk at all.
            //
            // Deliberately not "then look for a runtime this agent owns": that
            // guess is how an agent ends up on a sandbox nobody pointed it at.
            // An unresolved binding is re-resolved when the create lands.
            RuntimeChoice::Pending | RuntimeChoice::Inherit => AgentRuntime::Pending,
        }
    }

    /// The runtimes `owner` is responsible for — the ones it asked for, which
    /// are the ones that go away when it does. A sub session that inherited its
    /// parent's owns nothing and takes nothing down with it.
    pub fn runtimes_owned_by(
        &self,
        owner: Uuid,
    ) -> impl Iterator<Item = (RuntimeId, &RuntimeRecord)> {
        self.runtimes
            .iter()
            .filter(move |(_, r)| r.owner == owner)
            .map(|(id, r)| (*id, r))
    }

    /// The record for the session's own runtime — the one its main agent runs
    /// on. `None` for a runtime-less session, and for a workflow run's root.
    #[must_use]
    pub fn root_runtime(&self) -> Option<&RuntimeRecord> {
        self.runtimes.get(&self.forest.root_runtime_id()?)
    }

    /// Tokens banked across every agent this session hosts. Banked, so a turn
    /// in flight is not in it and nothing has to be asked of an agent.
    pub fn session_usage_total(&self) -> UsageTotal {
        self.agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u))
    }

    /// One agent's banked numbers, its subtree's included.
    ///
    /// `agent` is `None` for the main agent, whose usage is keyed by name
    /// rather than by uuid — the one place the two vocabularies meet, and the
    /// reason this takes the key apart rather than making callers assemble it.
    #[must_use]
    pub fn agent_stats(&self, agent: Option<Uuid>, key: &str) -> AgentStats {
        let usage = self.agent_usage.get(key).copied().unwrap_or_default();
        // The subtree walk needs a uuid; the main agent's is the session's,
        // which the forest roots itself by. Without one — a key the forest
        // does not know — the agent stands alone, which is what its own total
        // already says.
        let subtree_usage = match agent {
            Some(agent) => {
                self.forest
                    .descendant_agents(agent)
                    .into_iter()
                    .fold(usage, |acc, below| {
                        acc.combine(
                            &self
                                .agent_usage
                                .get(&below.to_string())
                                .copied()
                                .unwrap_or_default(),
                        )
                    })
            }
            None => usage,
        };
        AgentStats {
            usage,
            subtree_usage,
            context_tokens: self
                .agent_context_tokens
                .get(key)
                .copied()
                .unwrap_or_default(),
        }
    }

    /// The session's status, as a pure projection: a terminal failure, then
    /// the sandbox lifecycle, then the *root* entry's own phase. Child work
    /// never moves this — a subagent's invoked workflow failing is that
    /// subagent's news, not the session turning red.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        if let Some(reason) = &self.fatal {
            return SessionStatus::Unrecoverable {
                reason: reason.clone(),
            };
        }
        // The *session's own* runtime, not any of them: a sub session booting a
        // machine of its own is that sub session's news, and reporting it here
        // would paint the whole session as provisioning while its main agent
        // works. A session with no runtime skips this entirely.
        match self.root_runtime().map(|r| &r.provisioning) {
            Some(ProvisioningState::InFlight { .. }) => return SessionStatus::Provisioning,
            Some(ProvisioningState::Failed { reason, .. }) => {
                return SessionStatus::ProvisioningFailed {
                    reason: reason.clone(),
                };
            }
            Some(ProvisioningState::Never | ProvisioningState::Ready { .. }) | None => {}
        }
        match self.forest.root().map(|entry| &entry.state) {
            Some(RunState::Main(main)) => match &main.turn {
                TurnPhase::Idle => SessionStatus::Idle,
                TurnPhase::Running => SessionStatus::Running,
                TurnPhase::AwaitingInput => SessionStatus::AwaitingInput,
                TurnPhase::Failed { error } => SessionStatus::Failed {
                    reason: error.clone(),
                },
            },
            Some(RunState::Workflow(w)) => match w.run.status {
                // Pending and suspended both rest: a person decides what moves
                // them (the first step starts itself; a retry moves a
                // suspension).
                WorkflowRunStatus::Pending | WorkflowRunStatus::Suspended => SessionStatus::Idle,
                WorkflowRunStatus::Running => SessionStatus::Running,
                WorkflowRunStatus::AwaitingInput => SessionStatus::AwaitingInput,
                WorkflowRunStatus::Finished => SessionStatus::Finished,
                WorkflowRunStatus::Failed => SessionStatus::Failed {
                    reason: w.run.error.clone().unwrap_or_default(),
                },
            },
            // A root is only ever a session or a run; an unrooted session
            // has not been told what it is yet and rests.
            Some(RunState::Sub(_) | RunState::SubSession(_)) | None => SessionStatus::Idle,
        }
    }

    /// The failure a client is shown beside the status, derived from the same
    /// facts the status is.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        crate::sessions::spec::status_reason(&self.status())
    }

    /// The graph a run resolves its steps from: the run entry's own snapshot.
    #[must_use]
    pub fn run_graph(&self, run: RunId) -> Option<Arc<WorkflowRunSpec>> {
        self.forest.workflow(run).map(|w| w.graph.clone())
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
    /// The sub sessions branched out of this one, with the numbers only this
    /// actor holds.
    ///
    /// Their own list rather than more `agents`, for the reason the wire
    /// document keeps them apart: a sub session never ends, so it has no
    /// result and no end stamp, and folding it into a roster of things that do
    /// would mean a null in every entry that is not one. The supervisor keeps
    /// a parallel row per sub session for the session *list*; this is the same
    /// sub session with what the list has no use for.
    pub sub_sessions: Vec<SubSessionEntry>,
}

/// One sub session, as the session that hosts it can answer for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubSessionEntry {
    pub id: Uuid,
    /// The sub session this was branched from; `None` means the main agent.
    pub parent: Option<Uuid>,
    pub title: String,
    pub status: AgentStatus,
    pub created_at_ms: u64,
    /// The moment of its last status change — the end of its last turn once it
    /// is idle again. A session has no *end*, so this is not one.
    pub last_activity_ms: u64,
    /// The brief it was branched with.
    pub input: String,
    pub stats: AgentStats,
    /// The model it runs under, for the window lookup above this actor.
    pub model: Option<String>,
}

/// What became of one of a session's agents.
///
/// One vocabulary for three different underlying facts — a session's main
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
    /// Loaded and not working — where a session's agent rests between
    /// turns, and the one state that is not an ending.
    Idle,
    /// Parked on a question, waiting for an answer.
    AwaitingInput,
    /// Ran to a result. Only a subagent or a step reaches it: a session is
    /// never *done*.
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// The name a client reads this status by.
    ///
    /// Here rather than beside a wire type because two places now project an
    /// `AgentStatus` outward — the HTTP layer and the supervisor's global feed
    /// — and a second copy of this mapping is a second chance for them to
    /// disagree about what `awaiting_input` is called.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::AwaitingInput => "awaiting_input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
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
    /// What this agent is called: the main agent's title (and so the
    /// session's), a subagent's or sub session's, or the name of the step this
    /// agent is one execution of. `None` only while nothing has named it.
    pub title: Option<String>,
    pub depth: u32,
    /// The plugin-declared agent type a typed subagent runs as.
    pub agent_type: Option<String>,
    /// The saved preset this agent's settings were flattened from, when they
    /// were. Absent for an agent configured inline.
    ///
    /// Stamped by [`SessionActor::agent_roster`] rather than by the per-kind
    /// entry builders: only the actor can resolve settings for an arbitrary
    /// agent key, and doing it in one place is what stops main, step, subagent
    /// and sub session from each answering this differently.
    pub preset: Option<String>,
    pub status: AgentStatus,
    pub error: Option<String>,
    /// What kind of agent this is. On the entry rather than inferred from
    /// which fields happen to be set: a reader has to be able to tell a sub
    /// session from a subagent, and "it has no end stamp" is not a kind.
    pub kind: AgentKind,
    /// What this agent was asked to do: a subagent's task, a sub session's
    /// brief, a step's instruction. `None` for the main agent, which is asked
    /// things one turn at a time rather than briefed once.
    pub input: Option<String>,
    /// What it produced, once it has. Only work that owes a result has one —
    /// a subagent's report or a step's output. A main agent and a sub session
    /// answer their user turn by turn and conclude nothing.
    pub output: Option<String>,
    /// Its numbers, banked. Read from the session's own record, so a roster of
    /// thirty agents costs no recoveries.
    pub stats: AgentStats,
    /// The model alias it runs under — a step's own preset's, a subagent's
    /// inherited tree root's, the session's otherwise. Here so the HTTP layer
    /// can name the context window that goes with it; the actor cannot, since
    /// it does not know which models are configured.
    pub model: Option<String>,
    /// When this agent started and when it reached its result. Zero for a main
    /// agent — nothing spawned it, and it is as old as the session, whose
    /// `created_at` is on the same document — and zero for `ended_at_ms` while
    /// an agent is still running.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// The run this execution belongs to, and the workflow that run was
    /// started from. Set for a step and for nothing else.
    ///
    /// A session can host several runs — its own, if it is one, and one per
    /// `invoke_workflow` any of its agents called — and the roster lists every
    /// execution of all of them. Without this they are one flat list of steps,
    /// which is not enough to draw either run as the sequence it is.
    pub run: Option<Uuid>,
    pub workflow: Option<String>,
}

/// What kind of work one roster entry is.
///
/// The four things a session hosts, and they are genuinely four: only two of
/// them owe a result, only one of them is the session, and only one of them is
/// itself a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    /// The session's own agent. There is exactly one, and it is the session.
    Main,
    /// Delegated work: spawned with a task, owes its spawner a report.
    Sub,
    /// One execution of a workflow step. The definition chose it, not an agent.
    Step,
    /// A session branched out of this one. Talked to, owes nobody anything.
    SubSession,
}

/// One agent's numbers, as the session's own record has them.
///
/// Banked, never live: every field here is folded from an event this session
/// journaled, so the whole roster is answerable without waking a single agent.
/// The live figure — what an agent's context holds *mid-turn* — is on the
/// agent document, which is a read of the agent itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentStats {
    /// Tokens this agent alone has spent.
    pub usage: UsageTotal,
    /// `usage` plus every agent below it — the subagents it spawned, the sub
    /// sessions branched from it, the steps of the runs it invoked, and
    /// everything below those. What a folded node in the graph stands for.
    pub subtree_usage: UsageTotal,
    /// Tokens its context held at the end of its last turn.
    pub context_tokens: u32,
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
    /// The settings this agent runs under, resolved from what this session is
    /// and where the agent sits in it: a step's own preset, a subagent's
    /// inherited tree root, or the session's main settings.
    pub settings: AgentSettings,
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
    /// The main agent's usage, for the session kinds that have one. A run has
    /// no main agent, so it is `None` there.
    pub main_agent: Option<AgentUsageEntry>,
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
    /// One sub session of a session. Its own key for the same reason a step's
    /// is: nothing spawned it expecting a result, and it roots a subagent tree
    /// of its own.
    SubSession(Uuid),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every state has a spelling, and one spelling: a `_ =>` arm is how the
    /// documents that carry a status came to disagree about what a failed
    /// provision looks like. Three projections read this now — the session
    /// list, an agent document, and the global feed's sub session rows — so the
    /// mapping is tested where it lives rather than at one of them.
    #[test]
    fn every_agent_status_has_one_spelling() {
        for (status, expected) in [
            (AgentStatus::Provisioning, "provisioning"),
            (AgentStatus::Running, "running"),
            (AgentStatus::Idle, "idle"),
            (AgentStatus::AwaitingInput, "awaiting_input"),
            (AgentStatus::Completed, "completed"),
            (AgentStatus::Failed, "failed"),
            (AgentStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_wire(), expected);
        }
    }

    /// The container default fills anything a future version adds, so a bare
    /// snapshot still loads — the durability contract `#[serde(default)]`
    /// exists for.
    #[test]
    fn a_bare_session_state_deserializes_empty() {
        let state: SessionState = serde_json::from_str("{}").unwrap();
        assert_eq!(state.status(), SessionStatus::Idle);
        assert!(state.fatal.is_none());
        assert!(state.forest.root_id().is_none());
    }

    /// The one status a session cannot leave wins over everything else.
    #[test]
    fn fatal_projects_unrecoverable_over_any_root_phase() {
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            Uuid::new_v4(),
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        state.fatal = Some("runtime gone".into());
        assert_eq!(
            state.status(),
            SessionStatus::Unrecoverable {
                reason: "runtime gone".into()
            }
        );
        assert_eq!(state.last_error().as_deref(), Some("runtime gone"));
    }

    /// The sandbox lifecycle outranks the root's phase: nothing may look
    /// runnable while the runtime is still being built.
    #[test]
    fn provisioning_projects_over_an_idle_root() {
        let session = Uuid::new_v4();
        let runtime = RuntimeId::generate();
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        state.forest.point_at_runtime(
            session,
            crate::sessions::run_forest::RuntimeChoice::On(runtime),
        );
        fn set(state: &mut SessionState, runtime: RuntimeId, session: Uuid, p: ProvisioningState) {
            state.runtimes.insert(
                runtime,
                RuntimeRecord {
                    env: SessionSpec::for_vendor("mock")
                        .runtime_env()
                        .expect("a vendor spec has a runtime"),
                    owner: session,
                    provisioning: p,
                },
            );
        }
        set(
            &mut state,
            runtime,
            session,
            ProvisioningState::InFlight { at_ms: 5 },
        );
        assert_eq!(state.status(), SessionStatus::Provisioning);
        set(
            &mut state,
            runtime,
            session,
            ProvisioningState::Failed {
                at_ms: 5,
                reason: "vendor offline".into(),
            },
        );
        assert_eq!(
            state.status(),
            SessionStatus::ProvisioningFailed {
                reason: "vendor offline".into()
            }
        );
        set(
            &mut state,
            runtime,
            session,
            ProvisioningState::Ready { at_ms: 5 },
        );
        assert_eq!(state.status(), SessionStatus::Idle);
    }

    /// A session that asked for no runtime is never "provisioning": there is
    /// nothing being built, so its status is simply its root's phase. Without
    /// this, a runtime-less session would have to fake a `Ready` record to look
    /// idle.
    #[test]
    fn a_session_with_no_runtime_reads_as_its_root_phase() {
        let session = Uuid::new_v4();
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        assert!(state.runtimes.is_empty());
        assert_eq!(state.status(), SessionStatus::Idle);
    }
}
