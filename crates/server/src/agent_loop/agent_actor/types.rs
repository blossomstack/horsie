//! The vocabulary: what an agent is configured with, what it can be told, and
//! what it records.
//!
//! Nothing here decides anything. The commands are grouped the way
//! [`SessionCommand`](crate::sessions::session_actor::SessionCommand) is, one
//! group per module, so dispatch stays one line per module.

use super::*;
use crate::agent_loop::context::AgentRunDef;
use horsie_actor::ReplyTo;
use horsie_agentcore::{LifecycleEvent, Message, Usage};
use serde::{Deserialize, Serialize};

/// Per-agent configuration distilled from an [`AgentRunDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether this agent owes a structured result — true for a workflow step,
    /// which ends only by calling `submit_result`. Everything else finishes a
    /// turn with plain text, and that text *is* its answer.
    ///
    /// The one thing this decides: what a turn ending with text means. For a
    /// step it is either a park (something will wake it) or a mistake (nothing
    /// will); for anyone else it is the answer.
    pub requires_result: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Canonical thinking effort for this agent's runs, already resolved from
    /// the session's choice and the model's default. `None` sends no control.
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// Interactive (session) mode: recovery never injects a synthetic continue
    /// — the next user message is the continuation — and the event log is
    /// never snapshot-compacted (SSE cursors are journal sequence numbers and
    /// must stay stable). Workflow agents keep the default `false`.
    pub interactive: bool,
    /// Whether a turn ending with text is allowed to *conclude* while this
    /// agent still has delegated work in flight — subagents it spawned,
    /// workflows it invoked, timers it armed. True for a subagent: its
    /// conclusion is a report its parent consumes once, so an agent that is
    /// still waiting parks instead, and reports when its whole subtree is
    /// done. A step has the stronger `requires_result` gate; a session's
    /// final text is an answer to a person, not a report, and stays a turn
    /// boundary.
    pub park_on_outstanding_work: bool,
    /// The built-in tools this agent may call, by name. `None` is the default
    /// set, not "everything" — see [`crate::tools::resolve`].
    ///
    /// Carried down to the agent rather than applied by whoever built the
    /// toolbox, because the toolbox is only whole here: the actor is what
    /// stacks the timer and `task_list` layers on top of whatever it was
    /// handed.
    pub tools: Option<Vec<String>>,
}

impl AgentParams {
    pub fn from_def(def: &AgentRunDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            requires_result: false,
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
            thinking_effort: None,
            interactive: false,
            park_on_outstanding_work: false,
            tools: def.allowed_tools.clone(),
        }
    }
}

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
/// step runs is [`Components::advance`](super::component::Components::advance)'s
/// decision, and what it says is this component's.
pub enum RunCommand {
    /// Internal: one provider call finished — the assembled assistant message.
    StepDone {
        work: u64,
        response: Box<horsie_agentcore::StepResponse>,
    },
    /// Internal: one provider call failed.
    StepFailed {
        work: u64,
        error: horsie_agentcore::LlmError,
    },
    /// Internal: one dispatched tool call answered (or timed out inside its
    /// own toolbox). Carried per call rather than per batch so a fast tool's
    /// result is durable while a slow one still runs.
    ToolReturned {
        work: u64,
        tool_call_id: String,
        outcome: ToolReturn,
    },
    /// Internal: one chunk of the message a step is streaming. Unjournaled;
    /// carries the work generation so a cancelled step's stragglers are
    /// dropped instead of polluting the next message's delta buffer.
    StreamDelta { work: u64, text: String },
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
    ToolCall(ComponentToolCall),
    /// Internal: a timer's sleep elapsed.
    TimerFired {
        id: crate::agent_loop::timers::TimerId,
    },
}

/// The agent's own task list.
pub enum TaskListCommand {
    /// Internal: the turn routed the `task_list` tool here; answered by
    /// journaling its result exactly like a timer tool.
    ToolCall(ComponentToolCall),
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
    pub work: u64,
    pub outcome: Result<Box<TurnCtx>, crate::agent_loop::ContextError>,
}

/// Folding old history behind a summary boundary.
pub enum CompactionCommand {
    /// Internal: the spawned compaction run finished.
    Landed(Box<CompactLanding>),
}

/// What a compaction run needs and only its requester knows. Everything
/// shared — the provider, the budget, the hooks, the cancel token — is read
/// from the scratch's [`TurnCtx`], so nobody carries another component's
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
    pub work: u64,
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

/// Everything one turn's steps share, built once by the provision component
/// and published to the shared scratch for whoever needs it.
pub struct TurnCtx {
    pub provider: std::sync::Arc<dyn horsie_agentcore::LlmProvider>,
    /// The fully-composed, selection-filtered toolbox remote calls dispatch
    /// through. Component tools never reach it — the turn routes them to
    /// their components first.
    pub toolbox: std::sync::Arc<dyn horsie_agentcore::Toolbox>,
    /// What the model is shown, already filtered.
    pub specs: Vec<horsie_agentcore::ToolSpec>,
    /// The component-claimed tool names that survived the filter.
    pub inline_names: std::collections::HashSet<String>,
    pub system_prompt: String,
    pub budget: Option<horsie_agentcore::CompactionBudget>,
    pub conversation_id: String,
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// For the compaction hooks a compact run fires.
    pub context_provider: std::sync::Arc<dyn crate::agent_loop::ContextProvider>,
}

/// One tool call the turn routed to a component instead of the toolbox.
///
/// Carries the work generation so a component never acts for a turn that has
/// since been cancelled or superseded — the stale call is dropped, and the
/// cancel already repaired its dangling `tool_use`.
pub struct ComponentToolCall {
    pub work: u64,
    pub tool_call_id: String,
    pub name: String,
    pub input: serde_json::Value,
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
        after: Option<crate::agent_loop::agent_log::Cursor>,
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
        anchor: crate::agent_loop::agent_log::Anchor,
        max: usize,
        filter: crate::agent_loop::agent_log::LogFilter,
        reply: ReplyTo<crate::agent_loop::agent_log::LogPage>,
    },
    /// Find where in this log something was said. Answers positions and
    /// snippets, never entries — a caller reads what it found with
    /// [`Self::PageLog`].
    SearchLog {
        needle: String,
        max: usize,
        filter: crate::agent_loop::agent_log::LogFilter,
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
        work: u64,
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
    /// Internal: reconsider what this agent should be doing. The one thing
    /// any component may say, and it names nobody — see
    /// [`Components::advance`](super::component::Components::advance). Told by
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
    /// The generation the hooks ran under, so a cancel between the spawn and
    /// this landing drops it.
    pub work: u64,
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

/// Coarse events that alter persisted agent state. Streaming observation events
/// (text/tool-input deltas) are emitted to the event sink but never journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentDomainEvent {
    /// This agent was seeded from another session: `state` is the history it
    /// adopts, `seed` a synthetic message appended after it.
    ///
    /// One event rather than a snapshot written behind the actor's back, so a
    /// sub session's own journal explains where its history came from. Only
    /// ever the *first* event an agent has — replacing state wholesale is safe
    /// precisely because nothing has run.
    ///
    /// `seed` is `None` for every mode but a summary. A copy's history *is* the
    /// context and a fresh sub session's brief is queued behind this event, so
    /// only a summary has something to say that is nowhere else.
    ///
    /// Boxed: a whole session is far larger than any other variant here,
    /// and an enum is as big as its widest arm.
    Seeded {
        state: Box<AgentState>,
        #[serde(default)]
        seed: Option<Box<Message>>,
    },
    InputMessage {
        message: Message,
    },
    /// Queue items taken by work that is not a turn — a `/compact`, a summary
    /// for branching sub sessions. Journaled when the work *lands*, not when
    /// it starts: a crash in between replays the item, and doing it twice is
    /// cheaper than a sub session waiting for ever on a summary nobody will
    /// take again.
    Consumed {
        ids: Vec<String>,
        at_ms: u64,
    },
    MessageComplete {
        message: Message,
    },
    /// An assistant message the run never got to finish, rebuilt from the text
    /// it had already streamed when the turn was cancelled.
    ///
    /// A separate variant from `MessageComplete` because it is a different
    /// claim: the provider never said this message was done, and one day the
    /// history sent back may want to say so. It folds into the same log entry,
    /// because what was generated happened.
    MessageAborted {
        message: Message,
    },
    ToolComplete {
        tool_call_id: String,
        output: String,
        is_error: bool,
        /// What the tool produced beyond text — already-stored references.
        /// Journaled so the prompt the fold rebuilds carries them: the model's
        /// next call within the same turn must be able to see a screenshot a
        /// tool just took, and the fold is now the only source of history.
        #[serde(default)]
        artifacts: Vec<horsie_models::agent::ArtifactRef>,
        /// When the tool finished. Journaled rather than re-read at fold time:
        /// this variant rebuilds its `Message` in `apply_event`, so a recovered
        /// transcript would otherwise stamp every past tool result with the
        /// moment of recovery.
        at_ms: u64,
    },
    /// A plugin hook ran against a tool call this agent made. Journaled beside
    /// the call's own `ToolComplete` because a hook changes what the agent did,
    /// and that must be auditable rather than invisible.
    HookRan {
        record: horsie_models::hooks::HookRecord,
        /// How many records were already recorded against this same tool call.
        /// Journaled rather than recomputed at fold time so the id is a fact of
        /// the log: the live broadcast derives the entry from the event alone
        /// and would otherwise have to guess, giving the stream different
        /// cursors than `/history` for a call with more than one hook.
        seq: usize,
        at_ms: u64,
    },
    RunComplete {
        usage: Usage,
        iterations: u32,
        /// The last provider call's prompt size alone (not summed across
        /// iterations like `usage`) — what's actually in context now.
        context_tokens: u32,
        at_ms: u64,
    },
    /// A run that ended badly, and what it had spent by then.
    ///
    /// The accounting half of `RunComplete` without the turn half: no turn
    /// completed, so there is no `last_turn_usage` to set and no iteration
    /// count worth recording. Exactly one of the two ends a run, so folding
    /// both into `usage_total` cannot double-count.
    RunAborted {
        usage: Usage,
        context_tokens: u32,
        at_ms: u64,
    },
    RunCancelled {
        at_ms: u64,
    },
    /// A timer was armed.
    TimerArmed {
        record: crate::agent_loop::timers::TimerRecord,
        at_ms: u64,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::agent_loop::timers::TimerId>,
        at_ms: u64,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time
    /// for a recurring timer (so the fold stays pure); `None` removes a
    /// one-shot.
    TimerFired {
        id: crate::agent_loop::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
        at_ms: u64,
    },
    /// The agent parked, awaiting a timer or a subagent still working.
    Parked {
        at_ms: u64,
    },
    /// A turn ended without the result this agent owed, and nothing would have
    /// woken it. Journaled so the budget behind the nudge survives a restart —
    /// otherwise a crash loop hands the model a fresh nudge for ever.
    Nudged {
        at_ms: u64,
    },
    /// The task list changed (create/insert/update_status). Carries the full
    /// resulting state, not a delta — mirrors `MessageComplete`/`ToolComplete`,
    /// so replay never needs to re-derive or re-validate a past mutation.
    TaskListChanged {
        snapshot: crate::agent_loop::task_list::TaskListState,
        at_ms: u64,
    },
    /// Something that happened to the session, recorded here so there is one
    /// ordered record for a client to read rather than a second stream to
    /// reconcile against this one.
    ///
    /// The session actor still owns every one of these and remains the only
    /// thing that decides them; it tells the agent to record it. Journaled by
    /// the agent because the agent is the sole writer of its own log, which is
    /// what makes the order deterministic without any merge.
    LifecycleRecorded {
        event: LifecycleEvent,
        at_ms: u64,
    },
    /// Older history stopped being shown to the model.
    ///
    /// Journaled like any other append: the log keeps everything it held, and
    /// this only records where the prompt now starts. Folding it is therefore
    /// deterministic under replay, and a recovered agent prompts from exactly
    /// the boundary the live one did.
    ///
    /// Carries the *message id* the retained window starts at, not a seq. The
    /// run that produced it was holding a `Vec<Message>` in prompt order,
    /// which is not log order; resolving the two is the fold's job because the
    /// fold is the only thing holding the log.
    Compacted {
        summary: String,
        carried_state: String,
        retained_from_message_id: Option<String>,
        trigger: horsie_agentcore::CompactionTrigger,
        instructions: Option<String>,
        tokens_before: u32,
        tokens_after: u32,
        /// What the summarising call spent, aggregated into `usage_total` by
        /// the fold. Optional and defaulted: older boundaries carry none.
        #[serde(default)]
        usage: Option<Usage>,
        at_ms: u64,
    },
    /// Something was accepted into this agent's queue.
    ///
    /// Journaled before anything is done with it, which is what makes an
    /// accepted message a promise: it survives a crash and is still owed an
    /// answer.
    Received {
        item: crate::agent_loop::Incoming,
        at_ms: u64,
    },
    /// A turn began, consuming these queue items — and, if the agent was
    /// parked, answering these questions. One event so a crash anywhere in the
    /// window replays to the same place.
    TurnBegan {
        consumed: Vec<String>,
        /// Every question this turn *answered*. Empty when the turn abandoned
        /// them instead, which is what a plain message does.
        answered: Vec<String>,
        at_ms: u64,
    },
    /// The agent parked on these questions. One event for the whole park rather
    /// than one per question: they are asked together and answered together, so
    /// there is never a moment when only some of them are pending.
    AskRecorded {
        asks: Vec<crate::agent_loop::AskedQuestion>,
        at_ms: u64,
    },
    /// A summary was taken for sub sessions branching off this agent. Nothing
    /// about this agent's history changed; what the summarising call spent is
    /// aggregated into `usage_total` by the fold.
    SeedSummaryTaken {
        #[serde(default)]
        usage: Option<Usage>,
        at_ms: u64,
    },
}

/// One agent's own usage + context-size snapshot, with no message/task
/// payload — cheaper than [`AgentHistoryPage`] when only the numbers are
/// needed. Backs the session-level usage aggregation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentUsageSnapshot {
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
}

/// One agent's current values: the task list and its usage/context numbers.
/// Everything here is a value the client re-reads, never a log it accumulates —
/// which is why none of it rides on a history page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStateView {
    pub tasks: Vec<crate::agent_loop::task_list::TaskRecord>,
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
    /// The log position these values reflect, so a consumer holding a fold can
    /// tell whether this read is ahead of it or behind.
    pub as_of_seq: u64,
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
    use crate::agent_loop::agent_actor::testing::*;
    #[test]
    fn from_def_defaults_to_non_interactive() {
        assert!(!AgentParams::from_def(&def_fixture()).interactive);
    }

    /// Only a step owes a result. For everyone else a turn ending with plain
    /// text *is* the answer, and nudging one would be nonsense.
    #[test]
    fn from_def_owes_no_result() {
        assert!(!AgentParams::from_def(&def_fixture()).requires_result);
    }
}
