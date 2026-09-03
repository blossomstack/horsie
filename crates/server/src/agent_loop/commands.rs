//! What an agent can be told.
//!
//! Grouped the way
//! [`SessionCommand`](crate::sessions::session_actor::SessionCommand) is, one
//! group per component, so dispatch stays one line per component. Commands are
//! never journaled — only the events they decide are.
//!
//! Most of these are *internal*: a component's own spawned work reporting
//! back. The ones that arrive from outside are `Queue::Enqueue`,
//! `Queue::Answer`, the `Read` group, the `Log` group, `Seed`, and `Core`.

use crate::agent_loop::component::RoutedToolCall;
use crate::agent_loop::components::reads::ReadOutcome;
use crate::agent_loop::state::{AgentState, AgentStateView, AgentUsageSnapshot};
use crate::agent_loop::{AgentDomainEvent, TurnCtx};
use horsie_actor::ReplyTo;
use horsie_agentcore::{LifecycleEvent, Message, Usage};

/// Commands accepted by an [`AgentActor`].
///
/// Grouped the way
/// [`SessionCommand`](crate::sessions::session_actor::SessionCommand) is, and
/// for the same reason: the outer variant names the module that owns the
/// command, so dispatch is one line per module rather than one arm per
/// command.
pub enum AgentCommand {
    /// What this agent has been asked to answer, and the decision to answer it.
    Queue(QueueCommand),
    /// The turn in flight: stopping it, and what it writes and reports.
    Run(RunCommand),
    /// Timers this agent has armed against itself.
    Timer(TimerCommand),
    /// The agent's own task list.
    TaskList(TaskListCommand),
    /// The per-turn runtime and context setup.
    Provision(ProvisionCommand),
    /// Folding old history behind a summary boundary.
    Compaction(CompactionCommand),
    /// Questions answered from state, which wake nothing.
    Read(ReadCommand),
    /// Things written into this agent's log by somebody else.
    Log(LogCommand),
    /// Reading this session as a sub session's starting point, and being one.
    Seed(SeedCommand),
    /// The actor's own lifetime.
    Core(CoreCommand),
}

/// What this agent has been asked to answer, and the decision to answer it.
pub enum QueueCommand {
    /// Something addressed to this agent: a person's message, a subagent's
    /// report, a timer firing, a `Stop` hook's continuation.
    ///
    /// Durable *before* anything is done with it, and `ack` reports the write —
    /// so a caller that must know an accepted message will survive a crash
    /// (`POST /sessions/:id/messages`) can wait for that rather than trust a
    /// mailbox. Whether it becomes a turn is this agent's own decision, taken
    /// immediately afterwards; see [`crate::agent_loop::queued_turn`].
    Enqueue {
        item: crate::agent_loop::Incoming,
        ack: Option<ReplyTo<Result<(), horsie_actor::JournalError>>>,
    },
    /// Answer every question this agent is parked on, at once.
    ///
    /// All or nothing: a set that does not cover them exactly is refused and
    /// nothing is journaled. A half-answered park could not resume anyway — the
    /// next provider call would carry a `tool_use` with no result.
    Answer {
        answers: Vec<crate::agent_loop::AskAnswer>,
        reply: ReplyTo<Result<(), crate::agent_loop::AnswerError>>,
    },
    /// Internal: a turn's pre-start hooks finished. Journal their records, then
    /// commit the turn — or abandon it. Boxed to keep the command enum small.
    StartPrepared(Box<PreparedStart>),
}

/// What the turn's own spawned work reports back: one provider call, one
/// tool result, one streamed chunk. Nothing here starts anything — when a
/// step runs is [`AgentLoop::advance`](super::component::AgentLoop::advance)'s
/// decision, and what it says is this component's.
pub enum RunCommand {
    /// Internal: one provider call finished — the assembled assistant message.
    StepDone {
        marker_seq: u64,
        response: Box<horsie_agentcore::StepResponse>,
    },
    /// Internal: one provider call failed.
    StepFailed {
        marker_seq: u64,
        error: horsie_agentcore::LlmError,
    },
    /// Internal: one chunk of the message a step is streaming. Unjournaled and
    /// fenced by the open marker sequence.
    StreamDelta { marker_seq: u64, text: String },
    /// Internal: the Stop hook for one special step completed or timed out.
    StopHookDone {
        marker_seq: u64,
        result: crate::agent_loop::StopHookResult,
    },
    /// Internal: events to journal outside a step's own handler — recovery
    /// repairs, and tests. `ack` reports the durable write.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: ReplyTo<Result<(), horsie_actor::JournalError>>,
    },
}

/// What a dispatched tool call came back with.
pub enum ToolReturn {
    /// An ordinary result — including an error result, which the model reads.
    Result {
        output: String,
        is_error: bool,
        artifacts: Vec<horsie_models::agent::ArtifactRef>,
    },
    /// The call ended the run (`ask_user`, `submit_result`, ...). No result is
    /// recorded — the dangling `tool_use` is the shape of a parked agent.
    Stopped,
}

/// Timers this agent has armed against itself.
pub enum TimerCommand {
    /// Internal: the turn routed one of the timer tools here. The component
    /// executes it and journals both its own events and the call's result,
    /// which is all "answering" a tool call is — nothing is told to the turn.
    ToolCall(RoutedToolCall),
    /// Internal: a timer's sleep elapsed.
    TimerFired {
        id: crate::agent_loop::components::timers::domain::TimerId,
    },
}

/// The agent's own task list.
pub enum TaskListCommand {
    /// Internal: the turn routed the `task_list` tool here; answered by
    /// journaling its result exactly like a timer tool.
    ToolCall(RoutedToolCall),
}

/// The per-work runtime and context setup.
pub enum ProvisionCommand {
    /// Internal: the spawned setup finished. Asked for by the boundary, never
    /// by a component: provisioning serves a turn, a compaction and a summary
    /// identically, and none of them knows it happened.
    Provided(Box<ProvidedOutcome>),
}

/// What the spawned setup produced.
pub struct ProvidedOutcome {
    pub marker_seq: u64,
    pub initializing: bool,
    pub outcome: Result<Box<TurnCtx>, crate::agent_loop::ContextError>,
}

/// Folding old history behind a summary boundary.
pub enum CompactionCommand {
    /// Internal: the spawned compaction run finished.
    Landed(Box<CompactLanding>),
}

/// What a compaction run needs and only its requester knows. Everything
/// shared — the provider, the budget, the hooks, the cancel token — is read
/// from the step_run's [`TurnCtx`], so nobody carries another component's
/// context.
pub struct CompactJob {
    /// Queue items this compaction answers — a typed `/compact`. Empty when
    /// the boundary started it on its own.
    pub consumed: Vec<String>,
    pub manual: bool,
    pub instructions: Option<String>,
    /// The last provider call's prompt size — what the boundary records as
    /// `tokens_before`.
    pub tokens_before: u32,
}

/// What the spawned compaction run produced.
pub struct CompactLanding {
    pub marker_seq: u64,
    /// The queue items to cross off, journaled with the result.
    pub consumed: Vec<String>,
    /// What the summarising call spent, when one was made — journaled on the
    /// boundary event, never routed through anyone.
    pub usage: Option<Usage>,
    pub outcome: CompactOutcome,
}

pub enum CompactOutcome {
    /// A boundary to journal.
    Compacted(Box<CompactedData>),
    /// Nothing was folded — nothing to fold, a hook refused, or the summarise
    /// failed. `notice` says whether to tell the user (a typed `/compact`
    /// deserves an answer; the automatic check declining is routine).
    Skipped {
        notice: bool,
        context_tokens: u32,
        retain_tokens: Option<u32>,
    },
}

/// What a compaction run produced, ready to journal as
/// [`AgentDomainEvent::Compacted`].
pub struct CompactedData {
    pub summary: String,
    pub carried_state: String,
    pub retained_from_message_id: Option<String>,
    pub trigger: horsie_agentcore::CompactionTrigger,
    pub instructions: Option<String>,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

/// Questions answered from state, which wake nothing.
pub enum ReadCommand {
    /// Read forward from a cursor: durable entries plus, when the caller has
    /// caught up to the tail, the deltas of the message still being written.
    ///
    /// Answered from in-memory state — no journal access, no run. `after` of
    /// `None` means "from the very beginning", which is what a client with no
    /// position at all asks for.
    ReadLog {
        after: Option<crate::agent_loop::shared::agent_log::Cursor>,
        reply: ReplyTo<ReadOutcome>,
    },
    /// Read a bounded window at an anchor — scroll-back, or step forward
    /// through a run one page at a time. Separate from [`Self::ReadLog`]
    /// because it answers a different question and never carries deltas:
    /// nothing is being typed in the past.
    ///
    /// The filter is applied *here*, inside the actor that owns the log, so a
    /// caller asking for twenty user messages gets twenty rather than whatever
    /// survives out of the last twenty mixed entries. Filtering above this
    /// point cannot express that, whatever it does with the result.
    PageLog {
        anchor: crate::agent_loop::shared::agent_log::Anchor,
        max: usize,
        filter: crate::agent_loop::shared::agent_log::LogFilter,
        reply: ReplyTo<crate::agent_loop::shared::agent_log::LogPage>,
    },
    /// Find where in this log something was said. Answers positions and
    /// snippets, never entries — a caller reads what it found with
    /// [`Self::PageLog`].
    SearchLog {
        needle: String,
        max: usize,
        filter: crate::agent_loop::shared::agent_log::LogFilter,
        reply: ReplyTo<Vec<horsie_models::session_api::LogSearchHit>>,
    },
    /// The seq of the entry carrying this id, so a caller holding an id it saw
    /// quoted in some text can anchor a page on it. `None` when this log has no
    /// such entry.
    SeqOfId {
        id: String,
        reply: ReplyTo<Option<u64>>,
    },
    /// Where this agent's log stands. A sub session's branch point, read
    /// before anything is written so the number names the moment the sub
    /// session was asked for rather than the moment its seed happened to be
    /// built.
    LogHead { reply: ReplyTo<u64> },
    /// Read this agent's own usage + context-size snapshot — no messages or
    /// tasks, cheaper than `GetHistory` when only the numbers are needed.
    /// Backs the session-level usage aggregation.
    GetUsage { reply: ReplyTo<AgentUsageSnapshot> },
    /// Read this agent's current values — task list plus usage — for the agent
    /// document. Distinct from `GetHistory`, which returns transcript appends:
    /// these are values a client re-reads rather than accumulates.
    GetState { reply: ReplyTo<AgentStateView> },
}

/// Things written into this agent's log by somebody else.
pub enum LogCommand {
    /// Record something that happened to the session in this agent's log.
    ///
    /// Sent by the session actor, which still owns the fact — this only makes
    /// it visible in the one ordered thing a client reads. Journaled here
    /// because the agent is the sole writer of its own log, which is what makes
    /// the order deterministic with no merge anywhere.
    RecordLifecycle { event: LifecycleEvent, at_ms: u64 },
    /// Plugin hooks ran against one of this agent's tool calls. A `tell` with
    /// no ack: nothing waits on an audit trail, and recording what a hook did
    /// must never be able to slow the call it describes.
    HooksRan {
        records: Vec<horsie_models::hooks::HookRecord>,
    },
}

/// Reading this session as a sub session's starting point, and being one.
pub enum SeedCommand {
    /// This agent's state as a sub session's starting point, cut at `at_seq` —
    /// see [`AgentState::scrub_for_sub_session`]. Read-only: branching changes
    /// nothing about the session being branched.
    Snapshot {
        at_seq: u64,
        reply: ReplyTo<Box<AgentState>>,
    },
    /// Adopt `state` as this agent's whole history, append `seed` after it if
    /// there is one, and queue `message` — all in one write.
    ///
    /// Sent once, to a sub session, before it has run anything, which is what
    /// makes replacing state wholesale safe. Journaled as one batch rather
    /// than a snapshot written behind the actor's back, so the sub session's
    /// own log explains where its history came from.
    ///
    /// The message rides along rather than being enqueued separately for two
    /// reasons, both learned the hard way: enqueued first, the sub session
    /// drains and answers it *before* it has a history; enqueued after, a
    /// crash in between leaves a seeded sub session with nothing to do.
    SeedFrom {
        state: Box<AgentState>,
        seed: Option<Box<Message>>,
        message: crate::agent_loop::Incoming,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Internal: the spawned summary run finished.
    SummaryTaken {
        marker_seq: u64,
        consumed: Vec<String>,
        sub_sessions: Vec<uuid::Uuid>,
        result: Result<String, String>,
        /// What the summarising call spent — journaled by this component,
        /// never routed through anyone.
        usage: Option<Usage>,
    },
}

/// The actor's own lifetime.
pub enum CoreCommand {
    /// Internal: one dispatched tool call answered (or timed out inside its
    /// own toolbox). Carried per call rather than per batch so a fast tool's
    /// result is durable while a slow one still runs.
    ///
    /// A `Core` command because the *actor* dispatches tool calls and takes
    /// the answers — the turn only makes provider calls; what the model asked
    /// for is run at the level that holds the composed toolbox.
    ToolReturned {
        marker_seq: u64,
        tool_call_id: String,
        outcome: ToolReturn,
    },
    /// Internal: reconsider what this agent should be doing. The one thing
    /// any component may say, and it names nobody — see
    /// [`AgentLoop::advance`](super::component::AgentLoop::advance). Told by
    /// the actor after every durable write, and by hand where something
    /// changed without one.
    Advance,
    /// Stop whatever is in flight — a step, a compaction, a summary, or the
    /// setup before one. `ack`, if given, fires once nothing of this agent's
    /// is running any more; immediately when nothing was.
    ///
    /// The actor's own, not the turn's: a cancel means "stop", whatever the
    /// agent happens to be doing.
    Cancel { ack: Option<ReplyTo<()>> },
    /// Stop this actor. Sent when the session it belongs to unloads: the agent
    /// is resident for the session's *loaded* lifetime, not forever, and going
    /// cold must not leave a task behind holding a whole transcript in memory.
    Shutdown,
}

/// A turn whose pre-start hooks have run, on its way back to the actor.
///
/// Carries the drained turn untouched apart from a rewritten prompt: the
/// prepare step decides nothing about what the turn consumes, it only learns
/// what the hooks said.
pub struct PreparedStart {
    /// The open Agent marker the hooks prepare.
    pub marker_seq: u64,
    pub turn: crate::agent_loop::Turn,
    /// Records to journal before the turn snapshots its history — which is the
    /// whole reason this round-trip exists. Empty when no hook fired.
    pub records: Vec<horsie_models::hooks::HookRecord>,
    /// `Some` abandons the turn.
    pub abandon: Option<AbandonedStart>,
}

/// Why a prepared turn never ran.
pub enum AbandonedStart {
    /// A `UserPromptSubmit` hook refused the prompt. Deterministic for that
    /// prompt, so retrying it unchanged would be refused again.
    Blocked(String),
    /// Preparation could not complete — no runtime, most likely. The same
    /// failure `provide` would have reported one step later.
    Failed(crate::agent_loop::ContextError),
}
