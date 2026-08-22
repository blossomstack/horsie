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
    /// Internal: reconsider whether the queue may start a turn now. Sent after
    /// anything that could have changed the answer.
    Drain,
    /// Internal: a turn's pre-start hooks finished. Journal their records, then
    /// start the turn — or abandon it. Boxed to keep the command enum small.
    StartPrepared(Box<PreparedStart>),
}

/// The turn in flight: stopping it, and what it writes and reports.
pub enum RunCommand {
    /// Cancel an in-flight run. `ack`, if given, fires once the run has
    /// actually terminated — immediately when none is in flight — so a caller
    /// that must know this incarnation will write nothing more (e.g. a session
    /// about to spawn a replacement agent on the same journal) can wait for it
    /// rather than racing it.
    Cancel { ack: Option<ReplyTo<()>> },
    /// Internal: coarse events captured mid-run. `ack` lets the emitting loop
    /// await the durable write before continuing, so persistence applies
    /// backpressure on the agent loop, and reports the write outcome so a
    /// journal failure aborts the run instead of proceeding on an unrecorded
    /// history. Persistence still flows through this one mailbox.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: ReplyTo<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: a background run finished. Boxed to keep the command enum
    /// small.
    RunFinished(Box<RunReport>),
}

/// Timers this agent has armed against itself.
pub enum TimerCommand {
    /// Arm a timer; replies with the new timer id once recorded.
    ArmTimer {
        label: String,
        message: String,
        kind: crate::agent_loop::timers::TimerKind,
        after_secs: u64,
        reply: ReplyTo<crate::agent_loop::timers::TimerId>,
    },
    /// List active timers.
    ListTimers {
        reply: ReplyTo<Vec<crate::agent_loop::timers::TimerView>>,
    },
    /// Cancel one or all timers; replies with the ids actually removed.
    CancelTimer {
        selector: crate::agent_loop::timers::CancelSelector,
        reply: ReplyTo<Vec<crate::agent_loop::timers::TimerId>>,
    },
    /// Internal: a timer's sleep elapsed.
    TimerFired {
        id: crate::agent_loop::timers::TimerId,
    },
}

/// The agent's own task list.
pub enum TaskListCommand {
    /// Apply a `task_list` mutation (or just render `list`); durable like
    /// timers. Replies with the rendered list, or an error message if the
    /// action was rejected (unknown id, out-of-range position, ...).
    TaskListOp {
        action: crate::agent_loop::task_list::TaskListAction,
        reply: ReplyTo<Result<String, String>>,
    },
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
    /// Read a window *backwards* from a cursor — scroll-back. Separate from
    /// [`Self::ReadLog`] because it answers a different question and never
    /// carries deltas: nothing is being typed in the past.
    PageLog {
        before: Option<u64>,
        max: usize,
        reply: ReplyTo<crate::agent_loop::agent_log::LogPage>,
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
    /// The exact facts a compaction must carry across verbatim.
    ///
    /// Answered from state on the mailbox, and asked from a *running* agent's
    /// own task: a compaction can happen mid-turn, and the task list it must
    /// preserve may have been changed by a tool call earlier in that same turn.
    /// Reading a copy taken at run start would carry a stale one.
    CarriedState { reply: ReplyTo<String> },
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
    /// One chunk of the message currently being written.
    ///
    /// Routed through the mailbox rather than straight to readers so it is
    /// ordered against the entries around it: a chunk cannot overtake the entry
    /// it precedes, and the entry that supersedes it cannot land first. That
    /// ordering is the only reason this is a command at all — nothing here is
    /// journaled.
    RecordDelta { text: String },
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
}

/// The actor's own lifetime.
pub enum CoreCommand {
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
