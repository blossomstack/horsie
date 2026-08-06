use crate::context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, AskedQuestion, CONCLUDE_TOOL,
};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentLogBody, AgentLogEntry,
    AgentResult, ContentPart, EventSink, EventSinkError, HandoffCall, LifecycleEvent, LlmProvider,
    Message, Role, Toolbox, Usage,
};
use horsie_models::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Per-agent configuration distilled from an [`AgentRunDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether the agent produces structured output via `conclude`.
    pub has_output_schema: bool,
    /// Whether the agent may pause to ask the user.
    pub allow_ask_user: bool,
    /// Whether the agent may arm timers and park itself to await them.
    pub allow_timers: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Canonical thinking effort for this agent's runs, already resolved from
    /// the session's choice and the model's default. `None` sends no control.
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// Interactive (session) mode: recovery never injects a synthetic continue —
    /// the next user message is the continuation — and the event log is never
    /// snapshot-compacted (SSE cursors are journal sequence numbers and must
    /// stay stable). Workflow agents keep the default `false`.
    pub interactive: bool,
    /// An optional, never-forced handoff tool name, set by callers with their
    /// own terminal tool that isn't the workflow `conclude` mechanism above
    /// (e.g. the server crate's `ask_user` tool for interactive sessions). When
    /// set, this takes over from `handoff_tool()`/forced `conclude`: `tool_choice`
    /// stays `auto`, plain text is a perfectly normal reply, and a voluntary call
    /// to this tool is still recognized as a handoff. `None` for workflow agents.
    pub optional_handoff_tool: Option<String>,
}

impl AgentParams {
    pub fn from_def(def: &AgentRunDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            has_output_schema: def.output_schema.is_some(),
            allow_ask_user: def.allow_ask_user,
            allow_timers: def.allow_timers.unwrap_or(false),
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
            thinking_effort: None,
            interactive: false,
            optional_handoff_tool: None,
        }
    }

    /// The agent's handoff tool — the synthesized `conclude` tool when it has an
    /// output schema, may ask, or may park on timers, else `None` (plain text end).
    fn handoff_tool(&self) -> Option<String> {
        if self.has_output_schema || self.allow_ask_user || self.allow_timers {
            Some(CONCLUDE_TOOL.to_string())
        } else {
            None
        }
    }

    /// Every tool name a call to which *parks* this agent rather than running.
    ///
    /// A handoff tool is never executed: the run ends on the call and its result
    /// arrives later as an `InjectToolResult` (the user's answer to `ask_user`,
    /// a timer firing). So a dangling call to one is the normal shape of a
    /// parked agent, not the wreckage of an interrupted one — see
    /// [`missing_tool_results`], which must not journal a repair for it.
    fn handoff_tools(&self) -> Vec<String> {
        self.handoff_tool()
            .into_iter()
            .chain(self.optional_handoff_tool.clone())
            .collect()
    }
}

/// Commands accepted by an [`AgentActor`].
pub enum AgentCommand {
    /// Start a turn. `results` are tool results to record first — the answers to
    /// a park, or the results an abandoned park still owes the wire — and
    /// `message` is the user message that starts it.
    ///
    /// With no `message`, the results themselves are the turn's input, which is
    /// how an answered park resumes. With no `results`, it is an ordinary turn.
    /// At least one of the three must be present.
    ///
    /// `subagent_results` ride the user message alongside `message` as parts of
    /// their own, so a reader can tell a subagent's report from what the person
    /// typed. A turn carrying only these has no `message` at all.
    Resume {
        results: Vec<horsie_models::agent::ToolResultInput>,
        message: Option<String>,
        subagent_results: Vec<horsie_models::agent::SubAgentResultPart>,
    },
    /// Cancel an in-flight run. `ack`, if given, fires once the run has actually
    /// terminated — immediately when none is in flight — so a caller that must
    /// know this incarnation will write nothing more (e.g. a session about to
    /// spawn a replacement agent on the same journal) can wait for it rather
    /// than racing it.
    Cancel {
        ack: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// Internal: coarse events captured mid-run. `ack` lets the emitting loop await
    /// the durable write before continuing, so persistence applies backpressure on
    /// the agent loop, and reports the write outcome so a journal failure aborts the
    /// run instead of proceeding on an unrecorded history. Persistence still flows
    /// through this one mailbox.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: tokio::sync::oneshot::Sender<Result<(), horsie_actor::JournalError>>,
    },
    /// Plugin hooks ran against one of this agent's tool calls. A `tell` with no
    /// ack: nothing waits on an audit trail, and recording what a hook did must
    /// never be able to slow the call it describes.
    HooksRan {
        records: Vec<horsie_models::hooks::HookRecord>,
    },
    /// Internal: a turn's pre-start hooks finished. Journal their records, then
    /// start the turn — or abandon it. Boxed to keep the command enum small.
    StartPrepared(Box<PreparedStart>),
    /// Internal: a background run finished. Boxed to keep the command enum small.
    RunFinished(Box<RunReport>),
    /// Arm a timer; replies with the new timer id once recorded.
    ArmTimer {
        label: String,
        message: String,
        kind: crate::timers::TimerKind,
        after_secs: u64,
        reply: tokio::sync::oneshot::Sender<crate::timers::TimerId>,
    },
    /// List active timers.
    ListTimers {
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerView>>,
    },
    /// Cancel one or all timers; replies with the ids actually removed.
    CancelTimer {
        selector: crate::timers::CancelSelector,
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerId>>,
    },
    /// Internal: a timer's sleep elapsed.
    TimerFired { id: crate::timers::TimerId },
    /// Apply a `task_list` mutation (or just render `list`); durable like
    /// timers. Replies with the rendered list, or an error message if the
    /// action was rejected (unknown id, out-of-range position, ...).
    TaskListOp {
        action: crate::task_list::TaskListAction,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    /// Read forward from a cursor: durable entries plus, when the caller has
    /// caught up to the tail, the deltas of the message still being written.
    ///
    /// Answered from in-memory state — no journal access, no run. `after` of
    /// `None` means "from the very beginning", which is what a client with no
    /// position at all asks for.
    ReadLog {
        after: Option<crate::agent_log::Cursor>,
        reply: tokio::sync::oneshot::Sender<ReadOutcome>,
    },
    /// Read a window *backwards* from a cursor — scroll-back. Separate from
    /// [`Self::ReadLog`] because it answers a different question and never
    /// carries deltas: nothing is being typed in the past.
    PageLog {
        before: Option<u64>,
        max: usize,
        reply: tokio::sync::oneshot::Sender<crate::agent_log::LogPage>,
    },
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
    /// Watch this agent's position: `(tail_seq, delta_count)`.
    ///
    /// A `watch` rather than a broadcast of the data itself. It holds only the
    /// latest value and overwrites, so a slow reader cannot fall behind it —
    /// it simply sees a larger jump when it next looks. That is what makes
    /// overflow structurally impossible here rather than a case to handle:
    /// there is no buffer of pending items to overrun.
    WatchPosition {
        reply: tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<(u64, usize)>>,
    },
    /// Stop this actor. Sent when the session it belongs to unloads: the agent
    /// is resident for the session's *loaded* lifetime, not forever, and going
    /// cold must not leave a task behind holding a whole transcript in memory.
    Shutdown,
    /// Read this agent's own usage + context-size snapshot — no messages or
    /// tasks, cheaper than `GetHistory` when only the numbers are needed.
    /// Backs the session-level usage aggregation.
    GetUsage {
        reply: tokio::sync::oneshot::Sender<AgentUsageSnapshot>,
    },
    /// Read this agent's current values — task list plus usage — for the agent
    /// document. Distinct from `GetHistory`, which returns transcript appends:
    /// these are values a client re-reads rather than accumulates.
    GetState {
        reply: tokio::sync::oneshot::Sender<AgentStateView>,
    },
}

/// A turn whose pre-start hooks have run, on its way back to the actor.
///
/// Carries the original `Resume` payload untouched: the prepare step decides
/// nothing about the turn itself, it only learns what the hooks said.
pub struct PreparedStart {
    pub results: Vec<horsie_models::agent::ToolResultInput>,
    pub message: Option<String>,
    pub subagent_results: Vec<horsie_models::agent::SubAgentResultPart>,
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
    Failed(crate::ContextError),
}

/// What a live reader gets for one step forward.
///
/// Entries and deltas are answered together because they are two halves of one
/// position, and separating them would let a client hold a delta that belongs
/// after an entry it has not seen.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadOutcome {
    /// Durable entries after the caller's `entry_seq`.
    pub entries: Vec<AgentLogEntry>,
    /// Chunks of the message still being written, after the caller's
    /// `delta_seq`. Empty when the caller is behind the tail — live typing
    /// means nothing to a reader that has not caught up to the entry it
    /// follows.
    pub deltas: Vec<String>,
    /// The caller's delta position is impossible for the run now in flight, so
    /// it was talking to one that has since restarted. `deltas` therefore
    /// starts from the beginning and the caller must discard what it holds.
    pub reset_deltas: bool,
    /// Where the caller now is.
    pub cursor: crate::agent_log::Cursor,
}

impl ReadOutcome {
    /// Nothing new — the reader is exactly where the agent is.
    fn nothing(cursor: crate::agent_log::Cursor) -> Self {
        Self {
            entries: Vec::new(),
            deltas: Vec::new(),
            reset_deltas: false,
            cursor,
        }
    }

    /// Whether this outcome is worth sending. A wakeup can lose a race with
    /// another reader and find nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.deltas.is_empty() && !self.reset_deltas
    }
}

/// Coarse events that alter persisted agent state. Streaming observation events
/// (text/tool-input deltas) are emitted to the event sink but never journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentDomainEvent {
    InputMessage {
        message: Message,
    },
    MessageComplete {
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
    RunCancelled {
        at_ms: u64,
    },
    /// A timer was armed.
    TimerArmed {
        record: crate::timers::TimerRecord,
        at_ms: u64,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::timers::TimerId>,
        at_ms: u64,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for a
    /// recurring timer (so the fold stays pure); `None` removes a one-shot.
    TimerFired {
        id: crate::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
        at_ms: u64,
    },
    /// The agent parked itself awaiting its timers.
    Parked {
        at_ms: u64,
    },
    /// The task list changed (create/insert/update_status). Carries the full
    /// resulting state, not a delta — mirrors `MessageComplete`/`ToolComplete`,
    /// so replay never needs to re-derive or re-validate a past mutation.
    TaskListChanged {
        snapshot: crate::task_list::TaskListState,
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
}

/// The conversation history reconstructed by folding [`AgentDomainEvent`]s, plus
/// any timers the agent has armed and whether it is currently parked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// The transcript: everything the user sees, whether or not the model saw
    /// it. Read [`Self::prompt_messages`] to get what goes to a provider — this
    /// field deliberately cannot be handed to one.
    ///
    /// Every field here carries `#[serde(default)]`, including this one: state is
    /// snapshotted, so it is a durability contract. A field that fails to
    /// deserialize takes down `recover()` for every existing session — the way
    /// renamed event variants did on 2026-08-02. Add optional fields; never
    /// rename or repurpose one.
    ///
    /// This one has been renamed twice — from `messages: Vec<Message>` when the
    /// element type became a union, and from `history: Vec<HistoryEntry>` when
    /// entries gained a sequence number. Renaming rather than retyping in place
    /// is deliberate both times: serde ignores the now-unknown key and defaults
    /// this to empty, so an old snapshot yields an empty transcript instead of
    /// failing `recover()` and taking the supervisor down with it.
    #[serde(default)]
    pub log: Vec<AgentLogEntry>,
    /// The next `seq` to hand out.
    ///
    /// Deterministic across replay for the same reason `hook:{n}` is: the fold
    /// is deterministic, so re-running it produces the same numbers. Held in
    /// state rather than derived from `log.len()` so that front-trimming the
    /// log for context management stays possible without renumbering.
    #[serde(default)]
    pub next_seq: u64,
    /// Active timers — durable so they re-arm on recovery and back `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::timers::TimerRecord>,
    /// True while the agent has parked itself awaiting a timer (no run in flight).
    #[serde(default)]
    pub parked: bool,
    /// The agent's task list — durable so it survives an actor restart exactly
    /// like timers do; see `crate::task_list`.
    #[serde(default)]
    pub task_list: crate::task_list::TaskListState,
    /// Cumulative token usage across every completed run — durable agent state,
    /// folded from `RunComplete`. `u64` so a long session's re-sent-context input
    /// total can't overflow the per-turn `u32` wire counters. Answers the
    /// session's usage readout without replaying the whole journal.
    #[serde(default)]
    pub usage_total: UsageTotal,
    /// The most recently completed run's own usage — a per-run cost figure,
    /// summed across that run's tool-loop iterations but never across runs.
    /// `None` before this agent's first completed run.
    #[serde(default)]
    pub last_turn_usage: Option<Usage>,
    /// The most recently completed run's *last* provider call's prompt size
    /// alone (never summed) — what's actually loaded in this agent's context
    /// right now.
    #[serde(default)]
    pub context_tokens: u32,
}

/// Running token totals held in [`AgentState`]. Distinct from the per-turn wire
/// [`Usage`] (`u32`): this accumulates across all turns, so it is `u64` and owns
/// a `Default`, which the fluorite-generated `Usage` does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotal {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

impl UsageTotal {
    fn add(&mut self, usage: &Usage) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(u64::from(usage.input_tokens));
        self.output_tokens = self
            .output_tokens
            .saturating_add(u64::from(usage.output_tokens));
        self.cache_creation_tokens =
            add_optional(self.cache_creation_tokens, usage.cache_creation_tokens);
        self.cache_read_tokens = add_optional(self.cache_read_tokens, usage.cache_read_tokens);
    }

    /// Combines two agents' cumulative totals into a session-level aggregate.
    /// Only ever sums usage — never a context-size figure, which stays
    /// meaningfully per-agent (see `AgentUsageSnapshot::context_tokens`).
    pub fn combine(&self, other: &UsageTotal) -> UsageTotal {
        UsageTotal {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_tokens: combine_optional(
                self.cache_creation_tokens,
                other.cache_creation_tokens,
            ),
            cache_read_tokens: combine_optional(self.cache_read_tokens, other.cache_read_tokens),
        }
    }
}

/// Sums an accumulating `u64` cache total with a per-turn `u32` delta. Stays
/// `None` only when neither side has ever reported cache data.
fn add_optional(total: Option<u64>, delta: Option<u32>) -> Option<u64> {
    match (total, delta) {
        (None, None) => None,
        (total, delta) => Some(
            total
                .unwrap_or(0)
                .saturating_add(u64::from(delta.unwrap_or(0))),
        ),
    }
}

/// Sums two agents' `u64` cache totals. Stays `None` only when neither agent
/// has ever reported cache data.
fn combine_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// One agent's own usage + context-size snapshot, with no message/task
/// payload — cheaper than [`AgentHistoryPage`] when only the numbers are
/// needed. Backs the session-level usage aggregation.
#[derive(Debug, Clone, Default)]
pub struct AgentUsageSnapshot {
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
}

/// One agent's current values: the task list and its usage/context numbers.
/// Everything here is a value the client re-reads, never a log it accumulates —
/// which is why none of it rides on a history page.
#[derive(Debug, Clone, Default)]
pub struct AgentStateView {
    pub tasks: Vec<crate::task_list::TaskRecord>,
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
    /// The log position these values reflect, so a consumer holding a fold can
    /// tell whether this read is ahead of it or behind.
    pub as_of_seq: u64,
}

/// Build the transcript entry for one hook record.
///
/// The id is derived, never generated: `hook:{n}` where `n` counts the hook
/// entries already in this transcript. Journal replay therefore reproduces the
/// ids it produced live, which a uuid could not — and a recovered transcript
/// must page with the same cursors as the one it replaced.
pub fn hook_entry(
    record: horsie_models::hooks::HookRecord,
    seq: usize,
    at_ms: u64,
) -> horsie_agentcore::HookEntry {
    horsie_agentcore::HookEntry {
        id: hook_entry_id(seq),
        created_at_ms: at_ms,
        record,
    }
}

/// The cursor id of the `seq`-th hook entry in a transcript.
///
/// Counts entries rather than records-per-call, because not every record has a
/// call: `hook:{tool_call_id}:{n}` cannot name a `SessionStart`. The tool join
/// is unaffected — it goes through the record's own `ToolScope`, which is where
/// it belongs.
///
/// One function, two callers — the fold and the live broadcast — because the
/// stream and `/history` must name the same entry the same way.
#[must_use]
pub fn hook_entry_id(seq: usize) -> String {
    format!("hook:{seq}")
}

impl AgentState {
    /// How many hook entries this transcript already holds. The next one's
    /// `seq`.
    #[must_use]
    pub fn hook_entry_count(&self) -> usize {
        self.log
            .iter()
            .filter(|e| matches!(e.body, AgentLogBody::Hook(_)))
            .count()
    }

    /// Append `body` at the next sequence number.
    ///
    /// The single place a `seq` is handed out, so the fold cannot produce a gap
    /// or a duplicate by accident.
    fn push(&mut self, at_ms: u64, body: AgentLogBody) {
        self.log.push(AgentLogEntry {
            seq: self.next_seq,
            at_ms,
            body,
        });
        self.next_seq += 1;
    }

    /// The seq of the newest entry, or `None` for an empty log. The tail a
    /// cursor is compared against.
    #[must_use]
    pub fn tail_seq(&self) -> Option<u64> {
        self.log.last().map(|e| e.seq)
    }

    /// What the model sees: the transcript, with every hook entry translated
    /// into the message it injects — most translate to nothing.
    ///
    /// The only way to obtain a `Vec<Message>` from state. `self.history` cannot
    /// be handed to a provider because the element types differ, so every kind of
    /// entry must state what, if anything, it shows the model;
    /// [`crate::hook_translation::translate`] is where that is decided, in one
    /// exhaustive match, and any future non-model entry inherits the obligation.
    pub fn prompt_messages(&self) -> Vec<Message> {
        self.log
            .iter()
            .filter_map(|e| match &e.body {
                AgentLogBody::Llm(m) => Some(m.clone()),
                AgentLogBody::Hook(h) => crate::hook_translation::translate(h),
                // Every lifecycle variant, present and future. This arm is the
                // reason `Lifecycle` is one union rather than nine flattened
                // ones: provider isolation cannot be forgotten for a variant
                // added later.
                AgentLogBody::Lifecycle(_) => None,
            })
            .collect()
    }

    /// This agent's current values, for the agent document.
    pub fn state_view(&self) -> AgentStateView {
        AgentStateView {
            tasks: self.task_list.tasks().to_vec(),
            usage_total: self.usage_total,
            last_turn_usage: self.last_turn_usage.clone(),
            context_tokens: self.context_tokens,
            as_of_seq: self.tail_seq().unwrap_or(0),
        }
    }

    /// Answer a forward read from `after`, against the deltas now in flight.
    ///
    /// Three cases, and the third is the whole reason the cursor has two parts:
    ///
    /// - **Behind the tail.** Entries only. Live typing means nothing to a
    ///   reader that has not reached the entry those deltas follow, and sending
    ///   them would place chunks of a message above messages it comes after.
    /// - **Caught up.** The deltas past the caller's own position.
    /// - **Claiming more deltas than exist.** Impossible for the run now in
    ///   flight, so the caller was talking to one that has since restarted —
    ///   entry `N` is still the tail after a crash, but the new run starts its
    ///   deltas again from one. Answered with everything and a reset. A single
    ///   flat counter would have reissued the same numbers to different content
    ///   and nothing could have noticed.
    #[must_use]
    pub fn read_from(
        &self,
        after: Option<crate::agent_log::Cursor>,
        deltas: &[String],
    ) -> ReadOutcome {
        let tail = self.tail_seq();
        let Some(cursor) = after else {
            // No position at all: everything, from the beginning.
            return ReadOutcome {
                entries: crate::agent_log::page_from_start(&self.log).to_vec(),
                deltas: deltas.to_vec(),
                reset_deltas: false,
                cursor: crate::agent_log::Cursor {
                    entry_seq: tail.unwrap_or(0),
                    delta_seq: deltas.len(),
                },
            };
        };

        let entries = crate::agent_log::page_after(&self.log, cursor.entry_seq).to_vec();
        if !entries.is_empty() {
            // Behind the tail. The deltas belong after the entries this reader
            // is only now receiving, so they wait for the next step.
            return ReadOutcome {
                cursor: crate::agent_log::Cursor {
                    entry_seq: entries.last().map_or(cursor.entry_seq, |e| e.seq),
                    delta_seq: 0,
                },
                entries,
                deltas: Vec::new(),
                reset_deltas: false,
            };
        }

        if cursor.delta_seq > deltas.len() {
            return ReadOutcome {
                entries: Vec::new(),
                deltas: deltas.to_vec(),
                reset_deltas: true,
                cursor: crate::agent_log::Cursor {
                    entry_seq: cursor.entry_seq,
                    delta_seq: deltas.len(),
                },
            };
        }

        if cursor.delta_seq == deltas.len() {
            return ReadOutcome::nothing(cursor);
        }

        ReadOutcome {
            entries: Vec::new(),
            deltas: deltas[cursor.delta_seq..].to_vec(),
            reset_deltas: false,
            cursor: crate::agent_log::Cursor {
                entry_seq: cursor.entry_seq,
                delta_seq: deltas.len(),
            },
        }
    }

    /// This agent's own usage + context-size snapshot — always the full,
    /// current picture (unlike `history_page`, there is no tail/scroll-back
    /// distinction here).
    pub fn usage_snapshot(&self) -> AgentUsageSnapshot {
        AgentUsageSnapshot {
            usage_total: self.usage_total,
            last_turn_usage: self.last_turn_usage.clone(),
            context_tokens: self.context_tokens,
        }
    }
}

/// Result of a background run, sent back to the actor as [`AgentCommand::RunFinished`].
/// Coarse events are streamed separately and incrementally via
/// [`AgentCommand::PersistProgress`]; this carries only the terminal outcome.
pub struct RunReport {
    /// Which run this is the report of. A cancelled run is still unwinding when
    /// the next one may already have started, and a report that arrives after
    /// its run was superseded must be dropped rather than clearing the *new*
    /// run's handle and delivering the old run's outcome as if it were its own.
    run_id: u64,
    outcome: RunOutcome,
}

/// The in-flight run: its identity and the token that cancels it.
struct RunHandle {
    id: u64,
    cancel: CancellationToken,
}

#[derive(Debug)]
enum RunOutcome {
    /// Agent ended its turn with plain text (no `conclude` tool registered).
    Completed {
        text: String,
    },
    /// Agent called its handoff tool; `calls` are the raw inputs, one per call.
    Concluded {
        calls: Vec<HandoffCall>,
    },
    Cancelled,
    Failed {
        error: String,
        recoverable: bool,
    },
    /// Context preparation failed and the outcome was already delivered to the
    /// parent on the run task; the actor only needs to clear its `running` flag.
    AlreadyReported,
}

/// Events an agent may journal between snapshots before the next turn boundary
/// takes one.
///
/// An agent's state *is* its transcript, so a snapshot costs O(transcript) to
/// write — snapshotting every turn would be quadratic over a session. This
/// trades a bounded replay on recovery for a bounded write amplification.
const SNAPSHOT_EVERY_EVENTS: u64 = 200;

/// Observer of an agent's durable history, notified once per event that is both
/// journaled and folded into state.
///
/// This is how a live stream learns what happened without reading the journal:
/// the actor is the only thing that touches its own log, and this is the seam it
/// publishes through. Implementations must not block — they run on the actor's
/// mailbox — and must treat delivery as best-effort.
pub trait AgentObserver: Send + Sync {
    /// `state` is the state *after* `event` was folded, so an observer that needs
    /// the resulting message can read `state.messages.last()` rather than
    /// re-deriving it from the event.
    fn publish(&self, event: &AgentDomainEvent, state: &AgentState);
}

/// An agent run, modelled as an event-sourced actor. Each `Run`/`InjectToolResult`
/// drives a background `horsie_agentcore::Agent` loop; coarse events are journaled
/// incrementally so a crashed session recovers its conversation and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<RunHandle>,
    /// Where durable history is published, when anyone is listening. `None` for
    /// workflow agents, which have no live stream.
    observer: Option<Arc<dyn AgentObserver>>,
    /// Events journaled since a snapshot was last *requested*. Counting requests
    /// rather than confirmed writes means a failed snapshot simply waits another
    /// interval, which is the right instinct for an optimization: retrying hard
    /// against a failing journal helps nobody.
    events_since_snapshot: u64,
    /// Id of the next run to start. Monotonic for this actor's loaded lifetime,
    /// which is all the fence needs — a report can only be stale within it.
    next_run_id: u64,
    /// A timer fired while a run was in flight; consume it when the run parks.
    pending_wake: bool,
    /// A prepare step is in flight. Gates a second `Resume` exactly as `running`
    /// does: between `Resume` and `StartPrepared` no run exists yet, so
    /// `running` alone would let two turns through and land two runs on one
    /// journal.
    preparing: bool,
    /// Whether this agent load has fired its start hook. Deliberately **not**
    /// journaled — a rehydrated agent fires again, which is precisely what
    /// `source: "resume"` means.
    start_hook_fired: bool,
    /// Callers waiting to hear that the in-flight run has terminated (see
    /// [`AgentCommand::Cancel`]). Drained the moment `RunFinished` is handled —
    /// the run task sends that as its very last act, so every journal write it
    /// could make has already happened by then.
    cancel_acks: Vec<tokio::sync::oneshot::Sender<()>>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them.
    ///
    /// Deliberately not journaled and not part of the fold. A delta's useful
    /// life ends when the finished message arrives — under a second — and
    /// persisting one would put a write transaction on the critical path of
    /// every token for data nothing will ever read again.
    deltas: Vec<String>,
    /// `(tail_seq, delta_count)`, published for readers to wait on. Only the
    /// position travels through here; the data itself is read from state, which
    /// is what leaves nothing to overflow.
    position: tokio::sync::watch::Sender<(u64, usize)>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        Self {
            ctx,
            params,
            running: None,
            observer: None,
            events_since_snapshot: 0,
            next_run_id: 0,
            pending_wake: false,
            preparing: false,
            start_hook_fired: false,
            cancel_acks: Vec::new(),
            deltas: Vec::new(),
            position: tokio::sync::watch::Sender::new((0, 0)),
        }
    }

    /// Publish where this agent now is, waking every reader parked on it.
    ///
    /// Called after anything that moves either half of the position. Sending
    /// the same value twice is harmless — a reader that finds nothing new
    /// simply parks again.
    fn publish_position(&self, state: &AgentState) {
        let _ = self
            .position
            .send((state.tail_seq().unwrap_or(0), self.deltas.len()));
    }

    /// Same actor, publishing its durable history to `observer` — what a session
    /// agent needs and a workflow agent does not.
    pub fn with_observer(
        ctx: AgentRuntimeContext,
        params: AgentParams,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::new(ctx, params)
        }
    }

    /// Snapshot at a turn boundary, but only once enough events have accrued.
    ///
    /// Without this an agent that only ever converses — no ask, no park, no
    /// cancel — would never snapshot, and every recovery would stay a full
    /// replay of the whole transcript.
    fn snapshot_if_due(&mut self) -> CommandEffect<AgentDomainEvent> {
        if self.events_since_snapshot < SNAPSHOT_EVERY_EVENTS {
            return CommandEffect::none();
        }
        self.events_since_snapshot = 0;
        CommandEffect::snapshot()
    }

    /// The journal identity of an agent session: kind `"agent"`, id = the session
    /// UUID. Centralizes the kind so the workflow (e.g. fork) and the actor agree.
    pub fn persistence_id_for(session_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", session_id.to_string())
    }

    /// Refuse to begin a turn while one is already in flight — running, or still
    /// in its prepare step.
    ///
    /// `start_run` overwrites `self.running` with a fresh token, so a second start
    /// orphans the first run's cancel token and leaves two background loops
    /// persisting interleaved events into one journal — including two
    /// `tool_result`s for the same `tool_call_id`, which makes the provider 400 on
    /// every later turn (#61 item 3). Callers gate on session status, but that is a
    /// different actor's state; this is the invariant enforced where it lives.
    ///
    /// `preparing` is part of it because a turn between `Resume` and
    /// `StartPrepared` has no run yet: gating on `running` alone would let a
    /// second `Resume` straight through into the same collision.
    fn reject_if_busy(&self, command: &str) -> Option<CommandEffect<AgentDomainEvent>> {
        if self.running.is_none() && !self.preparing {
            return None;
        }
        tracing::warn!(
            command,
            preparing = self.preparing,
            "refusing to start a turn while one is already in flight"
        );
        Some(CommandEffect::none())
    }

    /// Journal a prepared turn's hook records, then start it — or abandon it.
    ///
    /// The records are folded into a local copy of state before the prompt is
    /// read, which is the whole point of the prepare step: `state` here is the
    /// pre-command snapshot, and a `SessionStart` record that is not folded in
    /// first would first reach the model on the *next* turn.
    async fn start_prepared(
        &mut self,
        prepared: PreparedStart,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let PreparedStart {
            results,
            message,
            subagent_results,
            records,
            abandon,
        } = prepared;

        let at_ms = now_ms();
        let mut events = Vec::new();
        let mut folded = state.clone();
        for (seq, record) in (state.hook_entry_count()..).zip(records) {
            let event = AgentDomainEvent::HookRan { record, seq, at_ms };
            folded = Self::apply_event(folded, event.clone());
            events.push(event);
        }

        if let Some(abandon) = abandon {
            // A preparation failure is reported exactly as the same failure
            // coming out of `provide` would be — `terminal` above all, which is
            // what tells the session its sandbox is gone for good rather than
            // merely unreachable. A refusal is neither: the prompt was read and
            // rejected, so retrying it unchanged would be rejected again.
            let (error, recoverable, terminal) = match abandon {
                AbandonedStart::Blocked(reason) => (reason, false, false),
                AbandonedStart::Failed(e) => (e.message, true, e.terminal),
            };
            self.ctx
                .parent
                .deliver(AgentOutcome::Failed {
                    session_id: self.ctx.session_id,
                    error,
                    recoverable,
                    terminal,
                })
                .await;
            // The records are still journaled: a user whose prompt was refused
            // must be able to see which plugin refused it and why.
            return CommandEffect::persist(events);
        }

        // The ids answered here are not dangling, whatever the recovered
        // history says: their results are in this very input.
        let answering: std::collections::HashSet<String> =
            results.iter().map(|r| r.tool_call_id.clone()).collect();
        // Sanitize on every turn start: a history recovered from a
        // mid-turn crash may carry dangling tool calls (a no-op when
        // well-formed).
        let mut history = repair_unanswered_tool_calls_except(folded.prompt_messages(), &answering);

        // Results that precede a user message belong to the history, not
        // to the input: the turn is started by what the user said.
        let starts_a_user_turn = message.is_some() || !subagent_results.is_empty();
        let agent_input = if starts_a_user_turn {
            if !results.is_empty() {
                let recorded = AgentInput::tool_results(results).to_message(now_ms());
                events.push(AgentDomainEvent::InputMessage {
                    message: recorded.clone(),
                });
                history.push(recorded);
            }
            AgentInput::user_message_with_results(
                new_message_id(),
                message.unwrap_or_default(),
                subagent_results,
            )
        } else {
            AgentInput::tool_results(results)
        };
        // Persist the input message here (not via the streaming sink), so a
        // turn-restarting provider retry that re-emits it can never
        // double-persist it into two consecutive user messages.
        events.push(AgentDomainEvent::InputMessage {
            message: agent_input.to_message(now_ms()),
        });
        self.start_run(agent_input, ctx, history);
        CommandEffect::persist(events)
    }

    fn start_run(&mut self, input: AgentInput, ctx: &ActorContext<Self>, history: Vec<Message>) {
        let cancel = CancellationToken::new();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.running = Some(RunHandle {
            id: run_id,
            cancel: cancel.clone(),
        });

        let self_ref = ctx.self_ref();
        let context_provider = self.ctx.context_provider.clone();
        let allow_timers = self.params.allow_timers;
        let configured_prompt = self.params.system_prompt.clone();
        // An explicit optional handoff tool (e.g. the server crate's `ask_user`
        // tool for interactive sessions) always wins over the workflow `conclude`
        // mechanism and is never forced.
        let (handoff_tool, force_handoff_choice) = match self.params.optional_handoff_tool.clone() {
            Some(name) => (Some(name), false),
            None => (self.params.handoff_tool(), true),
        };
        let max_iterations = self.params.max_iterations;
        let thinking_effort = self.params.thinking_effort;
        let max_retries = self.params.max_retries;
        let parent = self.ctx.parent.clone();
        let session_id = self.ctx.session_id;
        // The same value, named for the other job it does. `session_id` is this
        // agent's own identity, and only a *main* agent's identity is a session
        // id — a subagent or a workflow step carries its own uuid. Each has its
        // own history, and so its own cacheable prefix, which is exactly the
        // granularity a provider grouping requests by conversation wants.
        let conversation_id = session_id.to_string();

        tokio::spawn(async move {
            // Provide this run's contexts on the spawned task (never the mailbox):
            // rehydrate the runtime, reconnect MCP, scan the workspace. A failure
            // here is a recoverable run failure -- report it and stop, exactly as a
            // provider/tool error would.
            //
            // Cancellable, because this is the *most* likely place to hang: it
            // awaits an MCP connect, a workspace scan and a SessionStart hook, all
            // of which cross a process boundary. Leaving it outside the cancel
            // path meant a stalled peer wedged the run exactly where `Stop` could
            // not reach it — `halt()` gave up after its timeout and the task
            // leaked for the process lifetime (#61 item 5b).
            let provided = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            run_id,
                            outcome: RunOutcome::Cancelled,
                        })))
                        .await;
                    return;
                }
                provided = context_provider.provide() => provided,
            };
            let contexts = match provided {
                Ok(c) => c,
                Err(error) => {
                    parent
                        .deliver(AgentOutcome::Failed {
                            session_id,
                            error: error.message,
                            recoverable: true,
                            terminal: error.terminal,
                        })
                        .await;
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            run_id,
                            outcome: RunOutcome::AlreadyReported,
                        })))
                        .await;
                    return;
                }
            };
            // Timer-capable agents run with the timer control tools layered on; these
            // execute by `ask`ing this actor and are never sent to the sandboxed runtime.
            let toolbox: Arc<dyn Toolbox> = if allow_timers {
                Arc::new(TimerToolbox {
                    inner: contexts.toolbox,
                    actor: self_ref.clone(),
                })
            } else {
                contexts.toolbox
            };
            // `task_list` is always available, like `skill`/`inspect_workspace` --
            // it's a working-memory aid every agent can reach for, not a permission
            // that needs gating per agent.
            let toolbox: Arc<dyn Toolbox> = Arc::new(TaskListToolbox {
                inner: toolbox,
                actor: self_ref.clone(),
            });
            let system_prompt = contexts
                .system_prompt
                .or(configured_prompt)
                .unwrap_or_default();
            // The sink persists each coarse event by `ask`ing this actor and awaiting
            // the durable write, so the LLM loop has end-to-end backpressure:
            // `emit().await` does not return until the event is journaled. Persistence
            // still flows through the actor's single mailbox (`PersistProgress`),
            // never the journal directly.
            let sink: Arc<dyn EventSink> = Arc::new(PersistSink {
                actor: self_ref.clone(),
            });
            let outcome = run_with_retries(
                contexts.provider,
                toolbox,
                sink,
                conversation_id,
                system_prompt,
                handoff_tool,
                force_handoff_choice,
                max_iterations,
                max_retries,
                thinking_effort,
                history,
                input,
                cancel,
            )
            .await;
            // All coarse events were already persisted (each `emit` awaited its ack),
            // so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(RunReport {
                    run_id,
                    outcome,
                })))
                .await;
        });
    }

    /// Interpret a `conclude` payload (or plain-text completion) and deliver the
    /// outcome to the parent. The conversation events were already persisted
    /// incrementally via [`AgentCommand::PersistProgress`], so this only records the
    /// terminal transition and decides the actor's lifecycle.
    async fn handle_finished(
        &mut self,
        report: RunReport,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        // A report from a run that has already been superseded says nothing
        // about the run that is in flight now: clearing the handle on its word
        // would leave the live run unstoppable, and delivering its outcome
        // would tell the parent that a turn it never saw is over.
        if self.running.as_ref().map(|r| r.id) != Some(report.run_id) {
            tracing::warn!(
                run_id = report.run_id,
                current = ?self.running.as_ref().map(|r| r.id),
                "dropping the report of a superseded run"
            );
            return CommandEffect::none();
        }
        self.running = None;
        // Answered before any parent delivery below: a canceller is likely
        // blocking its own mailbox waiting on this, and those deliveries `tell`
        // into that same mailbox — replying first keeps the two from deadlocking.
        // The run task has already finished (this message is its last act), so
        // "it will write nothing more" is true now.
        for ack in self.cancel_acks.drain(..) {
            let _ = ack.send(());
        }
        let session_id = self.ctx.session_id;
        let parent = self.ctx.parent.clone();

        match report.outcome {
            RunOutcome::Completed { text } => {
                // No conclude tool: treat the final text as the output.
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        session_id,
                        usage_total: state.usage_total,
                    })
                    .await;
                parent
                    .deliver(AgentOutcome::Concluded {
                        session_id,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. Its whole
                // transcript stays in memory for the next turn and for history
                // reads, and nothing has to replay a journal to answer either.
                self.snapshot_if_due()
            }
            RunOutcome::Concluded { calls } => {
                match self.interpret(calls) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                session_id,
                                usage_total: state.usage_total,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Concluded { session_id, output })
                            .await;
                        self.snapshot_if_due()
                    }
                    Conclusion::Ask(asks) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                session_id,
                                usage_total: state.usage_total,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Asked { session_id, asks })
                            .await;
                        // Stay alive — InjectToolResult resumes this same session.
                        // Snapshot to compact the incrementally-persisted log.
                        // Unconditional now that no cursor is a journal position:
                        // history and streams read state, so compaction is invisible.
                        self.events_since_snapshot = 0;
                        CommandEffect::snapshot()
                    }
                    Conclusion::Park => self.park_or_resume(state, ctx, session_id, parent).await,
                }
            }
            RunOutcome::Cancelled => {
                // A cancelled tool call has no result and never will get one.
                // Journal the synthetic result now, where it belongs — directly
                // after the assistant message that made the call — rather than
                // recomputing it on a clone at the top of every later turn. The
                // journal is then a faithful record of what the model was shown,
                // and a mid-history dangle can no longer accumulate.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.prompt_messages(), &self.params.handoff_tools())
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // Snapshot to compact the incrementally-persisted log on cancel.
                self.events_since_snapshot = 0;
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::Failed {
                        session_id,
                        error,
                        recoverable,
                        // A run that failed inside the loop says nothing about
                        // whether the sandbox still exists.
                        terminal: false,
                    })
                    .await;
                // The partial conversation was already journaled incrementally, so the
                // failed session stays inspectable. The agent stays alive: a failed
                // turn is not a dead agent, and the next message reuses it.
                CommandEffect::none()
            }
            RunOutcome::AlreadyReported => {
                // Context preparation failed before the loop began; the failure was
                // already delivered to the parent. Stay alive so the next message
                // can retry against the same in-memory transcript.
                CommandEffect::none()
            }
        }
    }

    /// Decide whether a handoff payload is a final output, an ask, or a park.
    /// An `optional_handoff_tool` (e.g. the server crate's `ask_user` tool) is
    /// single-purpose — always an ask — so it bypasses `classify_conclusion`'s
    /// `has_output_schema`/`allow_ask_user`-based branching entirely, which
    /// exists only to disambiguate the workflow crate's multi-purpose `conclude`
    /// payload shape.
    fn interpret(&self, calls: Vec<HandoffCall>) -> Conclusion {
        if self.params.optional_handoff_tool.is_some() {
            return Conclusion::Ask(
                calls
                    .into_iter()
                    .map(|call| AskedQuestion {
                        tool_call_id: Some(call.tool_call_id),
                        question: call
                            .data
                            .get("question")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect(),
            );
        }
        // A forced handoff is one conclusion, and `validate_handoff` rejects a
        // turn that calls it twice — so there is exactly one call here.
        let Some(call) = calls.into_iter().next() else {
            return Conclusion::Output(Value::Null);
        };
        classify_conclusion(
            self.params.has_output_schema,
            self.params.allow_ask_user,
            self.params.allow_timers,
            call.data,
            Some(call.tool_call_id),
        )
    }

    /// Decide what a `park` conclusion means: an illegal park (no timers fails the
    /// run), an immediate resume (a timer fired during the run), or a real park
    /// (stay alive, status → Parked).
    async fn park_or_resume(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<Self>,
        session_id: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        if state.timers.is_empty() {
            parent
                .deliver(AgentOutcome::Failed {
                    session_id,
                    error: "agent parked with no active timers — nothing would ever wake it"
                        .to_string(),
                    recoverable: false,
                    terminal: false,
                })
                .await;
            return CommandEffect::stop();
        }
        if self.pending_wake {
            // A timer fired mid-run; go straight back to work instead of parking.
            self.pending_wake = false;
            let wake = AgentInput::user_message(
                new_message_id(),
                "A timer fired while you were busy — re-check now.".to_string(),
            );
            let input_event = AgentDomainEvent::InputMessage {
                message: wake.to_message(now_ms()),
            };
            self.start_run(wake, ctx, state.prompt_messages());
            return CommandEffect::persist(vec![input_event]);
        }
        parent.deliver(AgentOutcome::Parked { session_id }).await;
        self.events_since_snapshot = 0;
        CommandEffect::persist(vec![AgentDomainEvent::Parked { at_ms: now_ms() }]).and_snapshot()
    }

    /// A timer's sleep elapsed. Re-arm a recurring timer, then resume the agent with
    /// a wake message — unless a run is already in flight, in which case coalesce the
    /// wake and let the run consume it when it parks.
    async fn handle_timer_fired(
        &mut self,
        id: crate::timers::TimerId,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(record) = state.timers.iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = now_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::timers::TimerKind::Recurring => {
                let next = now.saturating_add(record.interval_secs.saturating_mul(1000));
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(next)
            }
            crate::timers::TimerKind::OneShot => None,
        };
        let fired = AgentDomainEvent::TimerFired {
            id,
            next_fire_at_unix_ms,
            at_ms: now,
        };

        if self.running.is_some() {
            // A run is in flight: record the fire (re-arm) and remember to wake when
            // the run parks. Multiple fires coalesce into one wake.
            self.pending_wake = true;
            return CommandEffect::persist(vec![fired]);
        }

        // Idle/parked: start a fresh run with the wake message.
        let wake = AgentInput::user_message(new_message_id(), record.wake_message(display_count));
        let input_event = AgentDomainEvent::InputMessage {
            message: wake.to_message(now_ms()),
        };
        self.start_run(wake, ctx, state.prompt_messages());
        CommandEffect::persist(vec![fired, input_event])
    }
}

/// Classify a `conclude` payload into the agent's terminal intent. With timers the
/// payload is always `kind`-tagged (`submit`/`park`/`ask`); without, it follows the
/// legacy (has_output, allow_ask) shape.
fn classify_conclusion(
    has_output_schema: bool,
    allow_ask_user: bool,
    allow_timers: bool,
    data: Value,
    tool_call_id: Option<String>,
) -> Conclusion {
    let extract_question = |d: &Value| {
        d.get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if allow_timers {
        let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
        return match kind {
            "park" => Conclusion::Park,
            "ask" => Conclusion::Ask(vec![AskedQuestion {
                tool_call_id,
                question: extract_question(&data),
            }]),
            _ => Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null)),
        };
    }
    match (has_output_schema, allow_ask_user) {
        // Kind-tagged union.
        (true, true) => {
            let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
            if kind == "ask" {
                Conclusion::Ask(vec![AskedQuestion {
                    tool_call_id,
                    question: extract_question(&data),
                }])
            } else {
                Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null))
            }
        }
        // Output only: the payload is the output.
        (true, false) => Conclusion::Output(data),
        // Ask only: the payload is a question.
        (false, true) => Conclusion::Ask(vec![AskedQuestion {
            tool_call_id,
            question: extract_question(&data),
        }]),
        // No conclude tool registered — shouldn't be reached via a handoff.
        (false, false) => Conclusion::Output(data),
    }
}

#[derive(Debug)]
enum Conclusion {
    Output(Value),
    /// One or more questions, all parked on together.
    Ask(Vec<AskedQuestion>),
    Park,
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.ctx.session_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    fn apply_event(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        match event {
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::MessageComplete { message } => {
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::HookRan { record, seq, at_ms } => {
                state.push(at_ms, AgentLogBody::Hook(hook_entry(record, seq, at_ms)));
            }
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
                at_ms,
            } => state.push(
                at_ms,
                AgentLogBody::Llm(Message::tool_result(tool_call_id, output, is_error, at_ms)),
            ),
            AgentDomainEvent::LifecycleRecorded { event, at_ms } => {
                state.push(at_ms, AgentLogBody::Lifecycle(event));
            }
            AgentDomainEvent::TimerArmed { record, .. } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids, .. } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
                ..
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            AgentDomainEvent::Parked { .. } => state.parked = true,
            AgentDomainEvent::TaskListChanged { snapshot, .. } => state.task_list = snapshot,
            AgentDomainEvent::RunComplete {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(usage);
            }
            AgentDomainEvent::RunCancelled { .. } => {}
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            AgentCommand::Resume {
                results,
                message,
                subagent_results,
            } => {
                if let Some(reason) = self.reject_if_busy("Resume") {
                    return reason;
                }
                if results.is_empty() && message.is_none() && subagent_results.is_empty() {
                    tracing::warn!("Resume with nothing to resume on; ignoring");
                    return CommandEffect::none();
                }
                let turn = crate::StartTurn {
                    // A fresh agent has nothing in its transcript; anything else
                    // was folded from a journal. No framework flag needed to
                    // tell a cold start from a rehydration.
                    start_source: (!self.start_hook_fired).then_some(if state.log.is_empty() {
                        horsie_models::runtime::SessionStartSource::Startup
                    } else {
                        horsie_models::runtime::SessionStartSource::Resume
                    }),
                    prompt: message.clone(),
                };
                let nothing_due = turn.start_source.is_none() && turn.prompt.is_none();
                if nothing_due || !self.ctx.context_provider.has_start_hooks() {
                    return self
                        .start_prepared(
                            PreparedStart {
                                results,
                                message,
                                subagent_results,
                                records: Vec::new(),
                                abandon: None,
                            },
                            state,
                            ctx,
                        )
                        .await;
                }
                self.preparing = true;
                // Set when the prepare task is *spawned*, not when it returns: a
                // failed prepare must not re-fire the start hook on the next
                // turn, which would inject its context a second time.
                self.start_hook_fired = true;
                let provider = self.ctx.context_provider.clone();
                let self_ref = ctx.self_ref();
                tokio::spawn(async move {
                    let prepared = match provider.start_hooks(turn).await {
                        Ok(prep) => PreparedStart {
                            abandon: crate::start_blocked(&prep.records)
                                .map(AbandonedStart::Blocked),
                            records: prep.records,
                            results,
                            // A rewritten prompt replaces the turn's input; an
                            // absent one leaves what the user actually sent.
                            message: prep.message.or(message),
                            subagent_results,
                        },
                        Err(error) => PreparedStart {
                            results,
                            message,
                            subagent_results,
                            records: Vec::new(),
                            abandon: Some(AbandonedStart::Failed(error)),
                        },
                    };
                    let _ = self_ref
                        .tell(AgentCommand::StartPrepared(Box::new(prepared)))
                        .await;
                });
                CommandEffect::none()
            }
            AgentCommand::StartPrepared(prepared) => {
                self.preparing = false;
                self.start_prepared(*prepared, state, ctx).await
            }
            AgentCommand::HooksRan { records } => {
                let at_ms = now_ms();
                // Counted here, against the state as it stands, and carried on
                // the event: `agent_frame` sees only the event, so deriving the
                // id at fold time would give the live stream different cursors
                // than `/history`.
                let mut seq = state.hook_entry_count();
                let events = records
                    .into_iter()
                    .map(|record| {
                        let event = AgentDomainEvent::HookRan { record, seq, at_ms };
                        seq += 1;
                        event
                    })
                    .collect();
                CommandEffect::persist(events)
            }
            AgentCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            AgentCommand::Cancel { ack } => {
                match (&self.running, ack) {
                    (Some(run), ack) => {
                        run.cancel.cancel();
                        // Answered when the run reports back, not now: the point of
                        // the ack is "the run is over", and it is still winding down.
                        self.cancel_acks.extend(ack);
                    }
                    // Nothing in flight (idle, or paused on a pending ask): the
                    // caller's guarantee already holds.
                    (None, Some(ack)) => {
                        let _ = ack.send(());
                    }
                    (None, None) => {}
                }
                CommandEffect::none()
            }
            AgentCommand::ArmTimer {
                label,
                message,
                kind,
                after_secs,
                reply,
            } => {
                let now = now_ms();
                let record = crate::timers::TimerRecord::arm(
                    label,
                    message,
                    kind,
                    std::time::Duration::from_secs(after_secs),
                    now,
                );
                let id = record.id.clone();
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(after_secs),
                );
                let _ = reply.send(id);
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed {
                    record,
                    at_ms: now_ms(),
                }])
            }
            AgentCommand::ListTimers { reply } => {
                let now = now_ms();
                let views = state.timers.iter().map(|t| t.view(now)).collect();
                let _ = reply.send(views);
                CommandEffect::none()
            }
            AgentCommand::CancelTimer { selector, reply } => {
                let ids: Vec<crate::timers::TimerId> = match selector {
                    crate::timers::CancelSelector::All => {
                        state.timers.iter().map(|t| t.id.clone()).collect()
                    }
                    crate::timers::CancelSelector::One(id) => {
                        if state.timers.iter().any(|t| t.id == id) {
                            vec![id]
                        } else {
                            vec![]
                        }
                    }
                };
                let _ = reply.send(ids.clone());
                if ids.is_empty() {
                    CommandEffect::none()
                } else {
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled {
                        ids,
                        at_ms: now_ms(),
                    }])
                }
            }
            AgentCommand::TimerFired { id } => self.handle_timer_fired(id, state, ctx).await,
            AgentCommand::RunFinished(report) => self.handle_finished(*report, state, ctx).await,
            AgentCommand::TaskListOp { action, reply } => {
                let mut next = state.task_list.clone();
                match next.apply(action) {
                    Ok(()) => {
                        let text = next.render();
                        let _ = reply.send(Ok(text));
                        CommandEffect::persist(vec![AgentDomainEvent::TaskListChanged {
                            snapshot: next,
                            at_ms: now_ms(),
                        }])
                    }
                    Err(msg) => {
                        let _ = reply.send(Err(msg));
                        CommandEffect::none()
                    }
                }
            }
            AgentCommand::RecordLifecycle { event, at_ms } => {
                CommandEffect::persist(vec![AgentDomainEvent::LifecycleRecorded { event, at_ms }])
            }
            AgentCommand::RecordDelta { text } => {
                self.deltas.push(text);
                self.publish_position(state);
                CommandEffect::none()
            }
            AgentCommand::ReadLog { after, reply } => {
                let _ = reply.send(state.read_from(after, &self.deltas));
                CommandEffect::none()
            }
            AgentCommand::PageLog { before, max, reply } => {
                let _ = reply.send(crate::agent_log::page_before(&state.log, before, max));
                CommandEffect::none()
            }
            AgentCommand::WatchPosition { reply } => {
                // Published before handing the receiver over, so a reader that
                // subscribes between two changes still learns where the agent
                // is rather than waiting for the next one.
                self.publish_position(state);
                let _ = reply.send(self.position.subscribe());
                CommandEffect::none()
            }
            AgentCommand::GetUsage { reply } => {
                let _ = reply.send(state.usage_snapshot());
                CommandEffect::none()
            }
            AgentCommand::GetState { reply } => {
                let _ = reply.send(state.state_view());
                CommandEffect::none()
            }
            AgentCommand::Shutdown => CommandEffect::stop(),
        }
    }

    /// After recovery, repair whatever the crash left half-done, and re-drive an
    /// interrupted session. An empty history means nothing ran yet (the workflow
    /// will send `Run`); otherwise the process died mid-turn, so re-enter the
    /// loop with a synthetic continuation message. That continuation is
    /// intentionally not persisted as a new turn boundary: if we crash again
    /// before progress, recovery simply re-synthesizes it.
    /// Publish what just became durable. This is the whole reason a live stream
    /// no longer reads the journal: by the time this runs the events are written
    /// and folded, so `state` already contains the messages they appended.
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        self.events_since_snapshot = self
            .events_since_snapshot
            .saturating_add(events.len() as u64);
        // An entry supersedes every chunk that preceded it — the finished
        // message says everything they were building towards — so the deltas
        // are dropped the moment one lands. This is also what keeps the delta
        // sub-sequence short and restartable: it counts within one entry, never
        // across the session.
        if events.iter().any(coarse_appends_an_entry) {
            self.deltas.clear();
        }
        self.publish_position(state);
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
        // Re-arm every surviving timer with its remaining delay (fires immediately if
        // already due). Do this whether parked or mid-run, so timers keep firing.
        let now = now_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
        // A tool call the dead process was running has no result and never will.
        // Record the repair once, here, where it still belongs at the end of the
        // transcript — recomputing it per turn instead is what let it drift into
        // the middle of a history nobody could then repair in place.
        let repairs = missing_tool_results(&state.prompt_messages(), &self.params.handoff_tools());
        if !repairs.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let _ = ctx
                .self_ref()
                .tell(AgentCommand::PersistProgress {
                    events: repairs
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect(),
                    ack,
                })
                .await;
        }
        // Interactive sessions never self-continue: the user's next message is
        // the continuation.
        if self.params.interactive {
            return;
        }
        // A parked agent waits for a timer — do not re-drive a turn.
        if state.parked {
            return;
        }
        if state.log.is_empty() {
            return;
        }
        let history = repair_unanswered_tool_calls(state.prompt_messages());
        self.start_run(
            AgentInput::user_message(new_message_id(), "continue the interrupted task"),
            ctx,
            history,
        );
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled in the actor; a stale fire (timer since cancelled)
/// is ignored there, so an un-cancellable sleep task is harmless.
fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::timers::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref.tell(AgentCommand::TimerFired { id }).await;
    });
}

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
struct TimerToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::timers::timer_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, horsie_agentcore::ToolCallError> {
        use crate::timers::{CancelSelector, TimerId, TimerKind};
        use horsie_agentcore::ToolCallError;
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".to_string(),
                        ));
                    }
                };
                let Some(after_secs) = input
                    .get("after_secs")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".to_string(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let Some(message) = input
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.message must be a non-empty string".to_string(),
                    ));
                };
                let id = self
                    .actor
                    .ask(|reply| AgentCommand::ArmTimer {
                        label,
                        message,
                        kind,
                        after_secs,
                        reply,
                    })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(serde_json::json!({ "timer_id": id.0 }))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::ListTimers { reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views)
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".to_string(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::CancelTimer { selector, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(serde_json::json!({ "cancelled": ids }))
            }
            _ => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

/// Wraps an agent's toolbox, adding the always-available `task_list` tool. It
/// executes by `ask`ing the owning [`AgentActor`] (never forwarded to the
/// sandboxed runtime), so its state is durable -- journaled and replayed
/// exactly like timers (see `crate::task_list`).
struct TaskListToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TaskListToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(crate::task_list::task_list_tool_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, horsie_agentcore::ToolCallError> {
        use horsie_agentcore::ToolCallError;
        if name != crate::task_list::TASK_LIST_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let action = crate::task_list::TaskListAction::from_input(&input)?;
        let result = self
            .actor
            .ask(|reply| AgentCommand::TaskListOp { action, reply })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
        result
            .map(Value::String)
            .map_err(ToolCallError::InvalidInput)
    }
}

/// Captures coarse agent events while forwarding every event to the inner sink.
/// Used only inside [`run_with_retries`] to locate the handoff tool-call id;
/// persistence (with backpressure) happens in the inner [`PersistSink`].
struct CapturingSink {
    inner: Arc<dyn EventSink>,
    captured: Mutex<Vec<AgentEvent>>,
}

impl CapturingSink {
    fn new(inner: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            captured: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.captured.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl EventSink for CapturingSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(event.clone());
        }
        // Propagate the inner sink's outcome so a durability failure aborts the run.
        self.inner.emit(event).await
    }
}

/// Persists each coarse domain event by `ask`ing the agent actor and awaiting the
/// durable write before returning — this is what gives the agent loop end-to-end
/// backpressure. Persistence flows through the actor's mailbox
/// ([`AgentCommand::PersistProgress`]), never the journal directly.
///
/// This is the only sink. There used to be a second one forwarding every event
/// to a broadcast so a live stream could accumulate its own copy of the
/// transcript; readers now read the agent's state instead, so the copy — and
/// the ordering problem between it and the original — is gone.
///
/// `InputMessage` is intentionally NOT persisted here: the actor persists the input
/// itself when handling `Run`/`InjectToolResult`, so a turn-restarting retry that
/// re-emits the input can never double-persist it into two consecutive user
/// messages.
struct PersistSink {
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl EventSink for PersistSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Some(coarse) = coarse_event(&event) {
            // Await the durable write and act on its outcome:
            // - Ok(Ok(()))  → journaled; proceed.
            // - Ok(Err(je)) → the journal write FAILED. Abort the run rather than
            //   continue on a history that was never recorded.
            // - Err(_)      → the actor has stopped (the run is being torn down), so
            //   there is nothing to persist to and nothing to wait for; drop quietly.
            match self
                .actor
                .ask(|ack| AgentCommand::PersistProgress {
                    events: vec![coarse],
                    ack,
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(je)) => {
                    return Err(EventSinkError(format!("journal write failed: {je}")));
                }
                Err(_actor_gone) => {}
            }
        }
        // Text chunks go through the same mailbox, unjournaled. `tell` rather
        // than `ask`: nothing durable happens, so there is nothing to wait for
        // — but it still travels the mailbox, which is what keeps a chunk from
        // overtaking the entry it precedes.
        if let AgentEvent::TextChunk(chunk) = &event {
            let _ = self
                .actor
                .tell(AgentCommand::RecordDelta {
                    text: chunk.text.clone(),
                })
                .await;
        }
        Ok(())
    }
}

/// Whether folding this event appends a log entry — i.e. consumes a `seq`.
///
/// Kept beside [`AgentState::apply_event`] deliberately: the two must agree, and
/// a variant added to one without the other would either strand deltas under an
/// entry that superseded them or clear them for an event that appended nothing.
fn coarse_appends_an_entry(e: &AgentDomainEvent) -> bool {
    matches!(
        e,
        AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
    )
}

/// Map a single streaming event to the coarse domain event that should be
/// persisted, or `None` for streaming noise and for `InputMessage` (see
/// [`PersistSink`]).
fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
    match e {
        AgentEvent::MessageComplete(ev) => Some(AgentDomainEvent::MessageComplete {
            message: ev.message.clone(),
        }),
        AgentEvent::ToolComplete(ev) => Some(AgentDomainEvent::ToolComplete {
            tool_call_id: ev.tool_call_id.clone(),
            output: ev.output.clone(),
            is_error: ev.is_error,
            // Carried on the streaming event, not re-read here: the in-memory
            // history already holds a message stamped with it.
            at_ms: ev.at_ms,
        }),
        AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
            usage: ev.usage.clone(),
            iterations: ev.iterations,
            context_tokens: ev.context_tokens,
            at_ms: ev.at_ms,
        }),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
        | AgentEvent::ToolExecuting(_) => None,
    }
}

/// What a synthetic result says stands in for a tool call that never finished.
const INTERRUPTED_RESULT: &str = "interrupted, no result was recorded";

/// The synthetic results a history is missing, in call order — the repair as
/// *messages to journal*, where [`repair_unanswered_tool_calls`] returns the
/// repaired history to put on the wire.
///
/// Called at the two moments a call becomes permanently unanswerable — a cancel
/// and a recovery — so the repair is recorded where it belongs, at the end of
/// the transcript as it stands. Nothing else needs to journal it: a call that is
/// still in flight is not missing a result, it just does not have one yet.
///
/// A call to one of `handoff_tools` is exempt. Those park the agent — the run
/// ends on the call and the result comes later via `InjectToolResult` — so from
/// a journal alone a parked `ask_user` is indistinguishable from a call the dead
/// process was running, and recovery used to "repair" it. The user's answer was
/// then appended to a synthetic result already bearing the same `tool_use_id`,
/// and every later turn 400d on the duplicate. Idle offload made that routine:
/// any ask left unanswered past the idle timeout unloads and reloads.
///
/// Not journaling the repair is safe because [`repair_unanswered_tool_calls`]
/// still patches the history put on the wire, so an abandoned park can never
/// reach a provider dangling.
fn missing_tool_results(messages: &[Message], handoff_tools: &[String]) -> Vec<Message> {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
            ContentPart::Text(_)
            | ContentPart::ToolCall(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc)
                if !answered.contains(tc.id.as_str()) && !handoff_tools.contains(&tc.name) =>
            {
                Some(tc.id.clone())
            }
            ContentPart::ToolCall(_)
            | ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_) => None,
        })
        .collect();
    if dangling.is_empty() {
        return Vec::new();
    }
    synthetic_results(dangling).collect()
}

/// Make a history well-formed for the provider: every `tool_use`, in *any*
/// assistant message, must have a matching `tool_result`. Any missing one (a
/// tool call interrupted by Stop or a crash) gets a synthetic error result so
/// the model can retry.
///
/// Repairing only the last assistant message is not enough. A Stop mid-turn
/// journals the assistant's tool call with no outcome (#45); once later turns
/// push that message off the end, a history rebuilt from the journal carries an
/// unanswered `tool_use` mid-history and the provider rejects *every* subsequent
/// turn with a 400 — the session is bricked until the journal is repaired.
///
/// Each repair is placed where the wire expects the result: directly after its
/// assistant message, joining any run of real results already following it —
/// never appended to the end of a history that has moved on to later turns.
///
/// Since [`missing_tool_results`] journals the repair at the moment a call
/// becomes unanswerable, this should now find nothing. It stays as the guard on
/// the one thing that must never reach a provider, and costs one pass over an
/// in-memory history.
fn repair_unanswered_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    repair_dangling(messages, &std::collections::HashSet::new())
}

/// [`repair_unanswered_tool_calls`] for the resume-from-ask path, where
/// `answering` are the tool calls this very command is supplying results for
/// (e.g. every `ask_user` of a parked turn). They are about to be answered for
/// real, so they are not
/// dangling: repairing it too would put *two* results on one `tool_use_id` — the
/// duplicate shape stricter providers reject outright, and pure noise for the
/// ones that don't.
fn repair_unanswered_tool_calls_except(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    repair_dangling(messages, answering)
}

fn repair_dangling(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    let mut answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_)
            | ContentPart::ToolCall(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_) => None,
        })
        .collect();
    answered.extend(answering.iter().cloned());

    // Insertion index → the call ids needing a synthetic result there.
    let mut repairs: std::collections::BTreeMap<usize, Vec<String>> =
        std::collections::BTreeMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Assistant {
            continue;
        }
        let dangling: Vec<String> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                ContentPart::ToolCall(_)
                | ContentPart::Text(_)
                | ContentPart::ToolResult(_)
                | ContentPart::Thinking(_)
                | ContentPart::SubAgentResult(_) => None,
            })
            .collect();
        if dangling.is_empty() {
            continue;
        }
        // Past the results this turn *did* record, so a partially-answered
        // parallel batch stays one contiguous run.
        let mut at = i + 1;
        while messages.get(at).is_some_and(|next| next.role == Role::Tool) {
            at += 1;
        }
        repairs.entry(at).or_default().extend(dangling);
    }
    if repairs.is_empty() {
        return messages;
    }

    let mut out =
        Vec::with_capacity(messages.len() + repairs.values().map(Vec::len).sum::<usize>());
    for (i, m) in messages.into_iter().enumerate() {
        if let Some(ids) = repairs.remove(&i) {
            out.extend(synthetic_results(ids));
        }
        out.push(m);
    }
    // Calls left dangling by the final assistant message land past the end.
    for (_, ids) in repairs {
        out.extend(synthetic_results(ids));
    }
    out
}

fn synthetic_results(ids: Vec<String>) -> impl Iterator<Item = Message> {
    ids.into_iter()
        .map(|id| Message::tool_result(id, INTERRUPTED_RESULT, true, now_ms()))
}

#[allow(clippy::too_many_arguments)]
async fn run_with_retries(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    conversation_id: String,
    system_prompt: String,
    handoff_tool: Option<String>,
    force_handoff_choice: bool,
    max_iterations: Option<u32>,
    max_retries: u32,
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
) -> RunOutcome {
    let mut attempt: u32 = 0;
    loop {
        // CapturingSink wraps the PersistSink: it records events only to locate the
        // handoff tool-call id; persistence (with backpressure) happens in PersistSink.
        let capture = CapturingSink::new(sink.clone());
        let config = AgentConfig {
            max_iterations: max_iterations.unwrap_or_else(|| AgentConfig::default().max_iterations),
            thinking_effort,
            ..AgentConfig::default()
        };
        let mut builder = Agent::builder(provider.clone(), toolbox.clone(), &conversation_id)
            .with_system_prompt(system_prompt.clone())
            .with_config(config)
            .with_history(history.clone());
        if let Some(name) = &handoff_tool {
            builder = if force_handoff_choice {
                builder.with_handoff_tool(name.clone())
            } else {
                builder.with_handoff_tool_optional(name.clone())
            };
        }

        let mut agent = match builder.build() {
            Ok(a) => a,
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        };

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                return match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Handoff(h) => RunOutcome::Concluded { calls: h.calls },
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) => {
                // Whether the failed attempt already wrote something durable.
                // `PersistSink` journals exactly the events `coarse_event` maps,
                // so this is the same test it applied — no proxy, no guessing.
                let journaled = captured.iter().any(|ev| coarse_event(ev).is_some());
                // Three independent conditions, all required:
                //
                // 1. Budget remains.
                // 2. The failure is transient. `LlmError` already distinguishes
                //    RateLimit / Overloaded / Network from a permanent ApiError,
                //    and this layer used to discard all of it — retrying a 401 or
                //    a 400 context-length error exactly as eagerly as a 429.
                // 3. Nothing durable was written. The retry rebuilds the turn from
                //    the ORIGINAL `history`, which does not contain the events the
                //    failed attempt persisted, so retrying after partial progress
                //    leaves a phantom turn in the transcript that the model never
                //    saw — replayed into every later turn (#61 item 21). This is
                //    the same "only retry when nothing has been emitted" rule the
                //    providers already apply to their own streams.
                if attempt < max_retries && e.is_transient() && !journaled {
                    attempt += 1;
                    // Honour a provider-supplied delay when there is one; the
                    // exponential backoff is the fallback, not the rule.
                    let delay = e
                        .retry_after()
                        .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << attempt.min(6))));
                    tracing::warn!(
                        error = %e,
                        attempt,
                        delay_ms = delay.as_millis(),
                        "transient provider error with nothing journaled; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                if journaled && e.is_transient() && attempt < max_retries {
                    tracing::warn!(
                        error = %e,
                        "not retrying: the attempt already journaled progress that a \
                         restart from the original history would duplicate"
                    );
                }
                return RunOutcome::Failed {
                    // Report the classification rather than assuming recoverable:
                    // a permanent failure shown as transient invites the user to
                    // retry something that can never succeed.
                    recoverable: e.is_transient(),
                    error: e.to_string(),
                };
            }
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
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
    // Shared no-op collaborators for tests that only exercise the actor's own
    // bookkeeping and never start a run.
    struct StubContext;
    #[async_trait]
    impl crate::ContextProvider for StubContext {
        async fn provide(&self) -> Result<crate::Contexts, crate::ContextError> {
            Err(crate::ContextError::retryable("no context"))
        }
    }
    struct StubParent;
    #[async_trait]
    impl AgentOutcomeSink for StubParent {
        async fn deliver(&self, _: AgentOutcome) {}
    }

    use super::*;
    use horsie_models::agent::{TextPart, ToolCallPart, ToolResultPart};

    fn user_msg(text: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "u".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    fn def_fixture() -> AgentRunDef {
        AgentRunDef {
            system_prompt: None,
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        }
    }

    #[test]
    fn from_def_defaults_to_non_interactive() {
        assert!(!AgentParams::from_def(&def_fixture()).interactive);
    }

    /// Without a turn-boundary snapshot an agent that only converses — no ask,
    /// no park, no cancel — never snapshots, and every recovery stays a full
    /// replay of the whole transcript.
    #[test]
    fn a_turn_boundary_snapshots_only_once_enough_events_have_accrued() {
        let session_id = uuid::Uuid::new_v4();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(StubContext),
            parent: Arc::new(StubParent),
            session_id,
        };
        let mut agent = AgentActor::new(ctx, AgentParams::from_def(&def_fixture()));

        assert!(
            !agent.snapshot_if_due().snapshots(),
            "a fresh agent has nothing worth snapshotting"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS - 1;
        assert!(
            !agent.snapshot_if_due().snapshots(),
            "one event short of the interval must not snapshot"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS;
        assert!(
            agent.snapshot_if_due().snapshots(),
            "reaching the interval snapshots at the turn boundary"
        );
        assert_eq!(
            agent.events_since_snapshot, 0,
            "the counter resets on request, so a failed write waits one interval"
        );
        assert!(
            !agent.snapshot_if_due().snapshots(),
            "and the very next turn does not snapshot again"
        );
    }

    /// The observer replaces journal replay: it must see every durable event,
    /// after the fold, with the resulting message already in state.
    #[tokio::test]
    async fn an_observer_sees_durable_appends_with_folded_state() {
        use crate::{ContextError, ContextProvider, Contexts};
        use horsie_actor::{InMemoryJournal, Journal, spawn_root};

        struct NoContext;
        #[async_trait]
        impl ContextProvider for NoContext {
            async fn provide(&self) -> Result<Contexts, ContextError> {
                Err(ContextError::retryable("no context"))
            }
        }
        struct DeafParent;
        #[async_trait]
        impl AgentOutcomeSink for DeafParent {
            async fn deliver(&self, _: AgentOutcome) {}
        }

        /// Records `(event, message-count-at-publish)` so the test can prove the
        /// fold already happened when the observer ran.
        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<(String, usize)>>,
        }
        impl AgentObserver for Recorder {
            fn publish(&self, event: &AgentDomainEvent, state: &AgentState) {
                let label = match event {
                    AgentDomainEvent::InputMessage { message } => {
                        format!("input:{}", message.id)
                    }
                    AgentDomainEvent::MessageComplete { message } => {
                        format!("complete:{}", message.id)
                    }
                    other => format!("other:{other:?}"),
                };
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((label, state.log.len()));
            }
        }

        let session_id = uuid::Uuid::new_v4();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let recorder = Arc::new(Recorder::default());
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(NoContext),
            parent: Arc::new(DeafParent),
            session_id,
        };
        let agent = spawn_root(
            AgentActor::with_observer(ctx, AgentParams::from_def(&def_fixture()), recorder.clone()),
            journal,
        );

        let one = user_msg("one");
        let two = user_msg("two");
        let (ack, ack_rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::PersistProgress {
                events: vec![
                    AgentDomainEvent::InputMessage {
                        message: one.clone(),
                    },
                    AgentDomainEvent::MessageComplete {
                        message: two.clone(),
                    },
                ],
                ack,
            })
            .await
            .unwrap();
        ack_rx.await.unwrap().unwrap();

        let seen = recorder.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (format!("input:{}", one.id), 2),
                (format!("complete:{}", two.id), 2),
            ],
            "both events publish once, and state is already folded when they do"
        );
    }

    /// The one seam the conversation id can regress at silently. Everything
    /// downstream is typed — the field is required, so a provider cannot be
    /// handed a request without one — but *which* id `start_run` reads is a
    /// plain assignment, and getting it wrong (a fresh uuid, the run id) costs
    /// only a colder prompt cache. Nothing fails, so nothing would catch it.
    #[tokio::test]
    async fn a_run_tells_the_provider_the_agent_s_own_id() {
        use horsie_actor::{InMemoryJournal, Journal, spawn_root};
        use horsie_agentcore::EmptyToolbox;
        use horsie_agentcore::testkit::MockProvider;

        struct MockContext(Arc<MockProvider>);
        #[async_trait]
        impl crate::ContextProvider for MockContext {
            async fn provide(&self) -> Result<crate::Contexts, crate::ContextError> {
                Ok(crate::Contexts {
                    provider: self.0.clone(),
                    toolbox: Arc::new(EmptyToolbox),
                    system_prompt: None,
                })
            }
        }
        /// Forwards outcomes so the test awaits the run's end rather than
        /// sleeping on it.
        struct ReportingParent(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
        #[async_trait]
        impl AgentOutcomeSink for ReportingParent {
            async fn deliver(&self, outcome: AgentOutcome) {
                let _ = self.0.send(outcome);
            }
        }

        let provider = MockProvider::text("done");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The agent's own identity: a session id for a main agent, its own uuid
        // for a subagent or a workflow step. Distinct from every other id in
        // scope, so a test that passes cannot be reading the wrong one.
        let session_id = uuid::Uuid::new_v4();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(MockContext(provider.clone())),
            parent: Arc::new(ReportingParent(tx)),
            session_id,
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let agent = spawn_root(
            AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
            journal,
        );

        agent
            .tell(AgentCommand::Resume {
                results: vec![],
                message: Some("hi".into()),
                subagent_results: vec![],
            })
            .await
            .unwrap();

        // `UsageRecorded` rides alongside the terminal outcome, so read until
        // the run itself reports.
        loop {
            match rx.recv().await.expect("the run must report an outcome") {
                AgentOutcome::UsageRecorded { .. } => continue,
                AgentOutcome::Concluded { .. } => break,
                other => panic!("expected the turn to conclude, got {other:?}"),
            }
        }

        let ids: Vec<String> = provider
            .requests()
            .into_iter()
            .map(|r| r.conversation_id)
            .collect();
        assert_eq!(
            ids,
            vec![session_id.to_string()],
            "the provider must be told this agent's own id, not any other"
        );
    }

    #[test]
    fn from_def_defaults_optional_handoff_tool_to_none() {
        assert!(
            AgentParams::from_def(&def_fixture())
                .optional_handoff_tool
                .is_none()
        );
    }

    // --- The pre-run hook seam ---
    //
    // `SessionStart` used to fire inside `provide()`, which runs on the run's
    // own task *after* the history snapshot — so a record journaled there first
    // reached the model on the following turn. These pin the seam that moved it
    // ahead of the snapshot, and the once-per-load bookkeeping that came with
    // it.

    mod start_hooks {
        use super::*;
        use horsie_actor::{ActorRef, InMemoryJournal, Journal, spawn_root};
        use horsie_agentcore::EmptyToolbox;
        use horsie_agentcore::testkit::MockProvider;
        use horsie_models::hooks::{
            ContextInjected, HookAction, HookBlocked, HookRecord, SessionStartOutcome,
            SessionStartRecord, UserPromptSubmitOutcome, UserPromptSubmitRecord,
        };
        use std::sync::Mutex;

        /// A provider that answers `start_hooks` from a script and records every
        /// `StartTurn` it was asked about.
        struct HookingContext {
            llm: Arc<MockProvider>,
            records: Vec<HookRecord>,
            enabled: bool,
            seen: Mutex<Vec<crate::StartTurn>>,
        }

        impl HookingContext {
            fn new(llm: Arc<MockProvider>, records: Vec<HookRecord>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records,
                    enabled: true,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn disabled(llm: Arc<MockProvider>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records: Vec::new(),
                    enabled: false,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn sources(&self) -> Vec<Option<String>> {
                self.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|t| t.start_source.as_ref().map(|s| s.as_wire().to_string()))
                    .collect()
            }
        }

        #[async_trait]
        impl crate::ContextProvider for HookingContext {
            async fn provide(&self) -> Result<crate::Contexts, crate::ContextError> {
                Ok(crate::Contexts {
                    provider: self.llm.clone(),
                    toolbox: Arc::new(EmptyToolbox),
                    system_prompt: None,
                })
            }

            fn has_start_hooks(&self) -> bool {
                self.enabled
            }

            async fn start_hooks(
                &self,
                turn: crate::StartTurn,
            ) -> Result<crate::TurnPreparation, crate::ContextError> {
                self.seen.lock().unwrap().push(turn);
                Ok(crate::TurnPreparation {
                    records: self.records.clone(),
                    message: None,
                })
            }
        }

        struct ReportingParent(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
        #[async_trait]
        impl AgentOutcomeSink for ReportingParent {
            async fn deliver(&self, outcome: AgentOutcome) {
                let _ = self.0.send(outcome);
            }
        }

        type Outcomes = tokio::sync::mpsc::UnboundedReceiver<AgentOutcome>;

        fn spawn(provider: Arc<HookingContext>) -> (ActorRef<AgentCommand>, Outcomes) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: provider,
                parent: Arc::new(ReportingParent(tx)),
                session_id: uuid::Uuid::new_v4(),
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = spawn_root(
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
                journal,
            );
            (agent, rx)
        }

        async fn prompt(agent: &ActorRef<AgentCommand>, text: &str, rx: &mut Outcomes) {
            agent
                .tell(AgentCommand::Resume {
                    results: vec![],
                    message: Some(text.into()),
                    subagent_results: vec![],
                })
                .await
                .unwrap();
            // `UsageRecorded` rides alongside the terminal outcome, so read
            // past it until the turn itself reports.
            while let AgentOutcome::UsageRecorded { .. } =
                rx.recv().await.expect("the turn must report an outcome")
            {}
        }

        fn session_start(context: &str) -> HookRecord {
            HookRecord {
                plugin: "boot".into(),
                duration_ms: 1,
                halt: None,
                action: HookAction::SessionStart(SessionStartRecord {
                    source: "startup".into(),
                    system_message: None,
                    outcome: SessionStartOutcome::Ran(ContextInjected {
                        additional_context: Some(context.into()),
                    }),
                }),
            }
        }

        /// The regression the whole seam exists to prevent: `provide()` runs
        /// after the run has already snapshotted its history, so a record
        /// journaled there would first appear on turn two — leaving every
        /// session's opening turn unhooked.
        #[tokio::test]
        async fn session_start_context_reaches_the_very_first_prompt() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider);

            prompt(&agent, "hi", &mut rx).await;

            let first = llm
                .requests()
                .into_iter()
                .next()
                .expect("one provider call");
            assert!(
                first.texts.iter().any(|t| t.contains("pins node 22")),
                "the first prompt must carry the start hook's context, got {:?}",
                first.texts
            );
        }

        /// `SessionStart` fired on every turn before this: `provide()` is
        /// per-run and its call had no guard, so every message re-ran every
        /// start hook and always reported `source: "startup"`.
        #[tokio::test]
        async fn a_second_turn_does_not_fire_the_start_hook_again() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;
            prompt(&agent, "again", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("startup".to_string()), None],
                "the start hook is due once per load; the prompt hook every turn"
            );
        }

        /// A rehydrated agent is a `resume`, and it is the only other lifecycle
        /// transition horsie has. Detected from the transcript rather than a
        /// framework flag: a fresh agent has nothing in it.
        #[tokio::test]
        async fn an_agent_with_recovered_history_reports_source_resume() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![]);
            let (agent, mut rx) = spawn(provider.clone());
            // Stand in for a recovered load: a transcript that predates this
            // actor's first command, which is exactly what folding a journal
            // leaves behind.
            let (ack, done) = tokio::sync::oneshot::channel();
            agent
                .tell(AgentCommand::PersistProgress {
                    events: vec![AgentDomainEvent::InputMessage {
                        message: user_msg("from a previous load"),
                    }],
                    ack,
                })
                .await
                .unwrap();
            done.await.unwrap().unwrap();

            prompt(&agent, "carry on", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("resume".to_string())],
                "a transcript that predates this load means the agent was recovered"
            );
        }

        /// A blocked prompt never becomes a turn: nothing is journaled as input
        /// and no run starts. The record still lands, so the user can see which
        /// plugin refused it.
        #[tokio::test]
        async fn a_blocked_prompt_journals_no_input_and_starts_no_run() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(
                llm.clone(),
                vec![HookRecord {
                    plugin: "guard".into(),
                    duration_ms: 1,
                    halt: None,
                    action: HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                        system_message: None,
                        outcome: UserPromptSubmitOutcome::Blocked(HookBlocked {
                            reason: Some("secrets in the prompt".into()),
                        }),
                    }),
                }],
            );
            let (agent, mut rx) = spawn(provider);

            agent
                .tell(AgentCommand::Resume {
                    results: vec![],
                    message: Some("my password is hunter2".into()),
                    subagent_results: vec![],
                })
                .await
                .unwrap();

            match rx.recv().await.expect("the turn must report an outcome") {
                AgentOutcome::Failed { error, .. } => {
                    assert_eq!(error, "secrets in the prompt");
                }
                other => panic!("expected the turn to be refused, got {other:?}"),
            }
            assert_eq!(llm.calls(), 0, "the model must never be reached");

            let page = agent
                .ask(|reply| AgentCommand::PageLog {
                    before: None,
                    max: 50,
                    reply,
                })
                .await
                .unwrap();
            assert_eq!(page.entries.len(), 1, "the record, and nothing else");
            assert!(matches!(page.entries[0].body, AgentLogBody::Hook(_)));
        }

        /// A preparation failure must classify itself exactly as the same
        /// failure out of `provide` would. Flattening `terminal` here leaves a
        /// session whose sandbox is gone for good reporting a retryable error,
        /// so it never reaches `Unrecoverable` and invites the user to try
        /// again forever.
        #[tokio::test]
        async fn a_terminal_preparation_failure_stays_terminal() {
            struct GoneContext;
            #[async_trait]
            impl crate::ContextProvider for GoneContext {
                async fn provide(&self) -> Result<crate::Contexts, crate::ContextError> {
                    Err(crate::ContextError::terminal("runtime is gone"))
                }
                fn has_start_hooks(&self) -> bool {
                    true
                }
                async fn start_hooks(
                    &self,
                    _: crate::StartTurn,
                ) -> Result<crate::TurnPreparation, crate::ContextError> {
                    Err(crate::ContextError::terminal("runtime is gone"))
                }
            }

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: Arc::new(GoneContext),
                parent: Arc::new(ReportingParent(tx)),
                session_id: uuid::Uuid::new_v4(),
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = spawn_root(
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
                journal,
            );
            agent
                .tell(AgentCommand::Resume {
                    results: vec![],
                    message: Some("hi".into()),
                    subagent_results: vec![],
                })
                .await
                .unwrap();

            match rx.recv().await.expect("the turn must report an outcome") {
                AgentOutcome::Failed { terminal, .. } => {
                    assert!(terminal, "a gone sandbox is terminal wherever it surfaces");
                }
                other => panic!("expected the turn to fail, got {other:?}"),
            }
        }

        /// A session with no plugins pays nothing for a seam it cannot use.
        #[tokio::test]
        async fn a_provider_without_start_hooks_makes_no_prepare_round_trip() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::disabled(llm.clone());
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;

            assert!(
                provider.sources().is_empty(),
                "`has_start_hooks() == false` must skip the round-trip entirely"
            );
            assert_eq!(llm.calls(), 1, "the turn still runs");
        }
    }

    #[test]
    fn a_replayed_tool_result_keeps_its_original_stamp() {
        // The stamp is journaled on the event rather than read from the clock
        // in `apply_event`; folding the same log twice must therefore produce
        // the same transcript, not one dated by whenever recovery happened.
        let fold = || {
            let mut state = AgentActor::initial_state();
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::ToolComplete {
                    at_ms: 1_700_000_000_123,
                    tool_call_id: "tc1".into(),
                    output: "result".into(),
                    is_error: false,
                },
            );
            state
        };
        let first = fold();
        let second = fold();
        assert_eq!(first.log[0].at_ms, 1_700_000_000_123);
        assert_eq!(first.log[0].at_ms, second.log[0].at_ms);
    }

    #[test]
    fn coarse_events_carry_the_stamp_the_agent_recorded() {
        let tool = coarse_event(&AgentEvent::ToolComplete(
            horsie_models::events::ToolCompleteEvent {
                message_id: "result:tc1".into(),
                tool_call_id: "tc1".into(),
                output: "ok".into(),
                is_error: false,
                at_ms: 42,
            },
        ))
        .expect("ToolComplete is journaled");
        assert!(
            matches!(tool, AgentDomainEvent::ToolComplete { at_ms, .. } if at_ms == 42),
            "the streaming event's stamp must survive into the journal"
        );

        let run = coarse_event(&AgentEvent::RunComplete(
            horsie_models::events::RunCompleteEvent {
                message_id: "run".into(),
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 1,
                at_ms: 99,
            },
        ))
        .expect("RunComplete is journaled");
        assert!(matches!(run, AgentDomainEvent::RunComplete { at_ms, .. } if at_ms == 99));
    }

    fn hook_record(plugin: &str, call: &str) -> horsie_models::hooks::HookRecord {
        horsie_models::hooks::HookRecord {
            plugin: plugin.to_string(),
            duration_ms: 3,
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

    fn with_hook(state: AgentState, plugin: &str, call: &str, seq: usize) -> AgentState {
        AgentActor::apply_event(
            state,
            AgentDomainEvent::HookRan {
                record: hook_record(plugin, call),
                seq,
                at_ms: 5,
            },
        )
    }

    /// A tool hook edits the tool's own output, so the tool result already
    /// represents whatever it did and there is nothing left to translate. If
    /// this ever reaches a provider it costs tokens on every call and repeats
    /// text the tool result already carries.
    #[test]
    fn a_tool_scoped_hook_entry_is_never_offered_to_the_model() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = with_hook(state, "guard", "tc1", 0);

        assert_eq!(state.log.len(), 2, "both entries are in the transcript");
        let prompt = state.prompt_messages();
        assert_eq!(prompt.len(), 1, "only the user message reaches the model");
        assert_eq!(prompt[0].role, Role::User);
    }

    /// The transcript is not the conversation: a translated entry keeps its
    /// place among the messages around it, so injected context lands where the
    /// hook ran rather than at the end of the prompt.
    #[test]
    fn a_translated_hook_entry_keeps_its_place_between_the_messages_around_it() {
        use horsie_models::hooks::{
            ContextInjected, HookAction, HookRecord, StopOutcome, StopRecord,
        };
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::HookRan {
                record: HookRecord {
                    plugin: "nagger".into(),
                    duration_ms: 1,
                    halt: None,
                    action: HookAction::Stop(StopRecord {
                        system_message: None,
                        outcome: StopOutcome::Ran(ContextInjected {
                            additional_context: Some("check the tests".into()),
                        }),
                    }),
                },
                seq: 0,
                at_ms: 2,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("carry on"),
            },
        );

        let prompt = state.prompt_messages();
        assert_eq!(prompt.len(), 3, "the hook contributes one message");
        assert_eq!(prompt[1].id, "hook-context:hook:0");
        assert!(
            matches!(&prompt[1].parts[0], ContentPart::Text(t) if t.text.contains("check the tests")),
            "the injected context reaches the model between the two messages"
        );
    }

    /// The id counts hook entries in the transcript, not records against a
    /// call: `hook:{tool_call_id}:{n}` cannot name a `SessionStart` record,
    /// which has no tool call. The tool join goes through the record's own
    /// `ToolScope` instead.
    #[test]
    fn hook_entry_ids_count_the_transcript_not_the_call() {
        let mut state = AgentActor::initial_state();
        state = with_hook(state, "guard", "tc1", 0);
        state = with_hook(state, "linter", "tc1", 1);
        state = with_hook(state, "guard", "tc2", 2);

        let ids: Vec<&str> = state.log.iter().filter_map(|e| e.body.id()).collect();
        assert_eq!(ids, vec!["hook:0", "hook:1", "hook:2"]);
    }

    /// `seq` is what the fold and the live broadcast agree on. Counting it from
    /// state at fold time instead would give a replayed transcript different
    /// ids than the stream, and a client's cursor would stop resolving.
    #[test]
    fn the_next_seq_counts_every_hook_entry() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.hook_entry_count(), 0);
        state = with_hook(state, "guard", "tc1", 0);
        state = with_hook(state, "linter", "tc1", 1);
        state = with_hook(state, "guard", "tc2", 2);

        assert_eq!(state.hook_entry_count(), 3);
    }

    /// A record with no tool call at all must reach the transcript: the locked
    /// decision "every hook that runs is recorded" was already untrue for
    /// `SessionStart`, which took a bespoke path returning a bare string.
    #[test]
    fn a_non_tool_record_is_a_transcript_entry_like_any_other() {
        use horsie_models::hooks::{
            ContextInjected, HookAction, HookRecord, SessionStartOutcome, SessionStartRecord,
        };
        let record = HookRecord {
            plugin: "boot".into(),
            duration_ms: 1,
            halt: None,
            action: HookAction::SessionStart(SessionStartRecord {
                source: "startup".into(),
                system_message: None,
                outcome: SessionStartOutcome::Ran(ContextInjected {
                    additional_context: Some("conventions".into()),
                }),
            }),
        };
        let state = AgentActor::apply_event(
            AgentActor::initial_state(),
            AgentDomainEvent::HookRan {
                record,
                seq: 0,
                at_ms: 7,
            },
        );
        assert_eq!(state.log.len(), 1);
        assert_eq!(state.log[0].body.id().unwrap(), "hook:0");
        let prompt = state.prompt_messages();
        assert_eq!(
            prompt.len(),
            1,
            "a session-start hook's context has nowhere else to live, so it \
             becomes a message"
        );
        assert_eq!(prompt[0].id, "hook-context:hook:0");
    }

    /// A page is a window over the log, hook entries included, and every entry
    /// consumes a number whatever kind it is — otherwise scroll-back would skip
    /// or stall on a hook row.
    ///
    /// The seq is what carries this now. The old id-keyed cursor had to reason
    /// about two disjoint id spaces (`result:{tool_call_id}` and `hook:{n}`) to
    /// stay unambiguous; one counter over all of them has nothing to disambiguate.
    #[test]
    fn the_log_numbers_every_kind_of_entry_in_one_sequence() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = with_hook(state, "guard", "tc1", 0);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::ToolComplete {
                tool_call_id: "tc1".into(),
                output: "denied".into(),
                is_error: true,
                at_ms: 9,
            },
        );

        assert_eq!(
            state.log.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "message, hook and tool result each take one number"
        );
        assert_eq!(state.next_seq, 3);

        let tail = crate::agent_log::page_before(&state.log, None, 2);
        assert_eq!(
            tail.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // The cursor resolves against a hook entry exactly like a message.
        let forward = crate::agent_log::page_after(&state.log, 1);
        assert_eq!(forward.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

        let back = crate::agent_log::page_before(&state.log, Some(1), 10);
        assert_eq!(
            back.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0]
        );
    }

    /// The property the whole design rests on: the same events fold to the same
    /// numbers, every time. Deterministic order comes from the agent being the
    /// sole writer of its own log, so replaying its journal has to reproduce
    /// exactly what ran live — otherwise a client's cursor means something
    /// different after a restart than it did before one.
    ///
    /// Asserted rather than argued, because nothing else would catch a fold
    /// that started numbering from a clock, a uuid, or `log.len()` on a
    /// front-trimmed log.
    #[test]
    fn folding_the_same_events_twice_produces_the_same_sequence() {
        let events = || {
            vec![
                AgentDomainEvent::InputMessage {
                    message: user_msg("hello"),
                },
                AgentDomainEvent::MessageComplete {
                    message: Message::user("a1", "hi", 2),
                },
                AgentDomainEvent::ToolComplete {
                    tool_call_id: "tc1".into(),
                    output: "ok".into(),
                    is_error: false,
                    at_ms: 3,
                },
                // Not an entry: it must not consume a number, or two replays
                // that differ only in timer activity would disagree.
                AgentDomainEvent::Parked { at_ms: 4 },
                AgentDomainEvent::LifecycleRecorded {
                    event: LifecycleEvent::TurnEnded(horsie_agentcore::TurnEndedLifecycle {
                        outcome: horsie_agentcore::TurnOutcome::Ended(
                            horsie_agentcore::EmptyOutcome {},
                        ),
                    }),
                    at_ms: 5,
                },
            ]
        };
        let fold = || {
            events()
                .into_iter()
                .fold(AgentActor::initial_state(), AgentActor::apply_event)
        };
        let shape = |s: &AgentState| -> Vec<(u64, Option<String>)> {
            s.log
                .iter()
                .map(|e| (e.seq, e.body.id().map(str::to_string)))
                .collect()
        };

        let first = fold();
        let second = fold();
        assert_eq!(shape(&first), shape(&second));
        assert_eq!(
            first.log.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "four entries; the park consumed no number"
        );
        assert_eq!(first.next_seq, 4);
        assert_eq!(first.next_seq, second.next_seq);
    }

    fn log_upto(n: u64) -> AgentState {
        (0..n).fold(AgentActor::initial_state(), |state, i| {
            AgentActor::apply_event(
                state,
                AgentDomainEvent::MessageComplete {
                    message: Message::user(format!("m{i}"), "x", i),
                },
            )
        })
    }

    fn chunks(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| (*s).to_string()).collect()
    }

    /// Live typing means nothing to a reader that has not reached the entry
    /// those chunks follow — sending them would draw fragments of a message
    /// above messages it comes after.
    #[test]
    fn a_reader_behind_the_tail_gets_entries_and_no_deltas() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_log::Cursor {
                entry_seq: 1,
                delta_seq: 0,
            }),
            &chunks(&["x", "y"]),
        );
        assert_eq!(
            out.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(out.deltas.is_empty());
        assert_eq!(out.cursor.entry_seq, 4);
        assert_eq!(out.cursor.delta_seq, 0);
    }

    #[test]
    fn a_caught_up_reader_gets_the_deltas_after_its_own() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 1,
            }),
            &chunks(&["x", "y", "z"]),
        );
        assert!(out.entries.is_empty());
        assert_eq!(out.deltas, vec!["y", "z"]);
        assert!(!out.reset_deltas);
        assert_eq!(out.cursor.delta_seq, 3);
    }

    /// The trap the two-part cursor exists to close.
    ///
    /// Entry 4 is still the tail after a crash, but the run that emitted the
    /// reader's 50 deltas is gone and the new one has emitted two. `50 > 2` is
    /// impossible for a live run, so the mismatch is arithmetic — and a single
    /// flat counter would have reissued 5..55 to different content with nothing
    /// able to notice.
    #[test]
    fn a_restarted_run_is_detected_and_answered_with_a_reset() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 50,
            }),
            &chunks(&["a", "b"]),
        );
        assert!(
            out.reset_deltas,
            "50 deltas cannot precede a run that has 2"
        );
        assert_eq!(out.deltas, vec!["a", "b"]);
        assert_eq!(out.cursor.delta_seq, 2);
    }

    #[test]
    fn a_reader_exactly_at_the_position_gets_nothing() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 2,
            }),
            &chunks(&["a", "b"]),
        );
        assert!(out.is_empty(), "a wakeup that lost a race sends nothing");
    }

    #[test]
    fn a_reader_with_no_cursor_gets_everything() {
        let state = log_upto(3);
        let out = state.read_from(None, &chunks(&["a"]));
        assert_eq!(
            out.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(out.deltas, vec!["a"]);
        assert_eq!(out.cursor.entry_seq, 2);
        assert_eq!(out.cursor.delta_seq, 1);
    }

    /// State is snapshotted, so a mixed transcript has to survive a round trip
    /// through serde or recovery loses every hook record it ever wrote.
    #[test]
    fn a_mixed_transcript_survives_a_snapshot_round_trip() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = with_hook(state, "guard", "tc1", 0);

        let json = serde_json::to_string(&state).unwrap();
        let back: AgentState = serde_json::from_str(&json).unwrap();

        // Both halves of an entry have to survive: the id it joins on and the
        // seq it is ordered by. A snapshot that kept the bodies but lost the
        // numbering would leave every live cursor pointing at the wrong entry.
        let shape = |s: &AgentState| -> Vec<(u64, Option<String>)> {
            s.log
                .iter()
                .map(|e| (e.seq, e.body.id().map(str::to_string)))
                .collect()
        };
        assert_eq!(shape(&back), shape(&state));
        assert_eq!(back.next_seq, state.next_seq);
        match &back.log[1].body {
            AgentLogBody::Hook(h) => {
                assert_eq!(h.record.plugin, "guard");
                // The externally-tagged union has to survive the round trip,
                // outcome and all — a snapshot is what a recovered transcript
                // is rebuilt from.
                match &h.record.action {
                    horsie_models::hooks::HookAction::PreToolUse(r) => {
                        assert_eq!(r.call.tool_call_id, "tc1");
                        match &r.outcome {
                            horsie_models::hooks::PreToolUseOutcome::Denied(d) => {
                                assert_eq!(d.reason.as_deref(), Some("not allowed"));
                            }
                            other => panic!("expected a denial, got {other:?}"),
                        }
                    }
                    other => panic!("expected a PreToolUse action, got {other:?}"),
                }
            }
            other => panic!("expected a hook entry, got {other:?}"),
        }
    }

    #[test]
    fn apply_event_rebuilds_history_in_order() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::MessageComplete {
                message: Message {
                    created_at_ms: 0,
                    started_at_ms: None,
                    id: "a".into(),
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    })],
                },
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::ToolComplete {
                at_ms: 0,
                tool_call_id: "tc1".into(),
                output: "result".into(),
                is_error: false,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 1,
            },
        );

        assert_eq!(state.log.len(), 3);
        let messages = state.prompt_messages();
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::Tool);
        match &messages[2].parts[0] {
            ContentPart::ToolResult(ToolResultPart {
                tool_call_id,
                output,
                ..
            }) => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(output, "result");
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn run_cancelled_is_noop_on_state() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hi"),
            },
        );
        let before = state.log.len();
        state = AgentActor::apply_event(state, AgentDomainEvent::RunCancelled { at_ms: 0 });
        assert_eq!(state.log.len(), before);
    }

    #[test]
    fn repair_appends_error_results_for_dangling_tool_calls() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false, 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        // tc2 was dangling → an error tool_result is appended at the end.
        let last = fixed.last().unwrap();
        match &last.parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "tc2");
                assert!(r.is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn answering_a_pending_ask_does_not_also_repair_it() {
        // The shape every ask_user answer resumes from: the call is dangling
        // *because* the user's answer is the result, arriving as this run's
        // input. Repairing it here would put a synthetic "interrupted" result
        // and the real answer on one tool_use_id.
        let history = vec![
            Message::user("m1", "pick a color", 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m2".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ];

        let answering = std::collections::HashSet::from(["ask1".to_string()]);
        let fixed = repair_unanswered_tool_calls_except(history.clone(), &answering);
        assert_eq!(fixed.len(), history.len(), "nothing is repaired: {fixed:?}");

        // Without the exclusion it *is* repaired — the bug this guards.
        assert_eq!(repair_unanswered_tool_calls(history).len(), 3);
    }

    /// The history an agent parked on an `ask_user` recovers from: the call is
    /// dangling because the user has not answered *yet*, not because anything
    /// died. Journaling a repair for it here is what put a synthetic
    /// "interrupted" result and the real answer on one `tool_use_id` — the
    /// duplicate every later turn then 400s on.
    fn parked_on_ask() -> Vec<Message> {
        vec![
            user_msg("what should I remove?"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ]
    }

    #[test]
    fn recovery_does_not_repair_the_ask_the_session_is_parked_on() {
        let handoff = vec!["ask_user".to_string()];
        assert!(
            missing_tool_results(&parked_on_ask(), &handoff).is_empty(),
            "a parked ask is awaiting its answer, not interrupted"
        );
        // Without the exemption it *is* repaired — the bug this guards, which
        // bricked every session offloaded while awaiting an answer.
        assert_eq!(missing_tool_results(&parked_on_ask(), &[]).len(), 1);
    }

    #[test]
    fn an_interactive_sessions_ask_tool_is_a_handoff_tool() {
        // The wiring the recovery exemption depends on: the server sets
        // `ask_user` here, and nothing else tells the agent that call parks it.
        let mut params = AgentParams::from_def(&def_fixture());
        params.optional_handoff_tool = Some("ask_user".to_string());
        assert_eq!(params.handoff_tools(), vec!["ask_user".to_string()]);
    }

    #[test]
    fn a_timer_parked_agent_exempts_its_conclude_call() {
        let mut def = def_fixture();
        def.allow_timers = Some(true);
        assert_eq!(
            AgentParams::from_def(&def).handoff_tools(),
            vec![CONCLUDE_TOOL.to_string()]
        );
    }

    #[test]
    fn recovery_still_repairs_a_real_tool_call_left_dangling_beside_a_park() {
        let mut history = parked_on_ask();
        history.insert(1, assistant_call("a0", "died"));
        let repairs = missing_tool_results(&history, &["ask_user".to_string()]);
        let ids: Vec<String> = repairs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["died".to_string()],
            "only the dead call is repaired"
        );
    }

    #[test]
    fn a_park_is_never_journaled_as_interrupted_but_is_still_repaired_on_the_wire() {
        // The safety net that makes not journaling the repair safe: an ask that
        // really is abandoned still reaches the provider well-formed.
        let history = parked_on_ask();
        assert!(missing_tool_results(&history, &["ask_user".to_string()]).is_empty());
        assert!(
            unmatched_tool_uses(&repair_unanswered_tool_calls(history)).is_empty(),
            "the wire history must never carry a dangling tool_use"
        );
    }

    /// Every `tool_use` id in `messages` that has no matching `tool_result`
    /// anywhere — what the provider rejects a request for.
    fn unmatched_tool_uses(messages: &[Message]) -> Vec<String> {
        let answered: std::collections::HashSet<&str> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(tc.id.as_str()) => {
                    Some(tc.id.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn assistant_call(id: &str, call_id: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            })],
        }
    }

    /// The session-bricking case: a Stop left a dangling call mid-history, and
    /// later turns pushed it off the end. Sanitizing only the last assistant
    /// message leaves it unrepaired, and the provider 400s on every later turn.
    #[test]
    fn repair_fixes_dangling_tool_calls_before_the_last_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"), // Stop landed here: no result ever journaled
            user_msg("never mind, do this instead"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a3".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
            },
        ];
        let fixed = repair_unanswered_tool_calls(history);
        assert!(
            unmatched_tool_uses(&fixed).is_empty(),
            "dangling calls left in rebuilt history: {:?}",
            unmatched_tool_uses(&fixed)
        );
    }

    /// The repair must land where the wire expects a result — right after the
    /// assistant message that made the call — not appended to the end of a
    /// history that has moved on to later turns.
    #[test]
    fn repair_places_synthetic_result_next_to_its_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"),
            user_msg("never mind"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[2].parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "stopped");
                assert!(r.is_error);
            }
            other => panic!("expected the synthetic result at index 2, got {other:?}"),
        }
        assert_eq!(fixed[2].role, Role::Tool);
    }

    /// A partially-answered parallel batch: the synthetic result joins the run
    /// of real results, still ahead of the next user turn.
    #[test]
    fn repair_appends_to_an_existing_run_of_tool_results() {
        let history = vec![
            user_msg("do both"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false, 0),
            user_msg("stop, do something else"),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[3].parts[0] {
            ContentPart::ToolResult(r) => assert_eq!(r.tool_call_id, "tc2"),
            other => panic!("expected tc2's result after tc1's, got {other:?}"),
        }
        assert_eq!(fixed.last().unwrap().role, Role::User);
    }

    #[test]
    fn repair_leaves_well_formed_history_untouched() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                })],
            },
            Message::tool_result("tc1", "ok", false, 0),
        ];
        let before = history.len();
        let fixed = repair_unanswered_tool_calls(history);
        assert_eq!(fixed.len(), before);
    }

    #[test]
    fn classify_park_kind_when_timers_enabled() {
        use serde_json::json;
        // timers on: a kind=park payload classifies as Park.
        let c = classify_conclusion(true, true, true, json!({"kind": "park"}), None);
        assert!(matches!(c, Conclusion::Park));
        // kind=submit classifies as Output(output field).
        let c = classify_conclusion(
            true,
            true,
            true,
            json!({"kind": "submit", "output": {"x": 1}}),
            None,
        );
        match c {
            Conclusion::Output(v) => assert_eq!(v["x"], 1),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn timer_events_fold_into_state() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;

        let rec = TimerRecord::arm(
            "pr".into(),
            String::new(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        let id = rec.id.clone();
        let mut state = AgentActor::initial_state();

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: rec,
            },
        );
        assert_eq!(state.timers.len(), 1);

        // Recurring fire re-arms in place with a carried next fire time and bumped count.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                at_ms: 0,
                id: id.clone(),
                next_fire_at_unix_ms: Some(120_000),
            },
        );
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].fire_count, 1);
        assert_eq!(state.timers[0].fire_at_unix_ms, 120_000);

        // One-shot fire (None) removes it.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                at_ms: 0,
                id,
                next_fire_at_unix_ms: None,
            },
        );
        assert!(state.timers.is_empty());
    }

    #[test]
    fn task_list_events_fold_into_state() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.task_list.render(), "No tasks.");

        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::task_list::TaskListAction::Create {
                tasks: vec!["a".to_string(), "b".to_string()],
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
        assert!(state.task_list.render().contains("[ ] 1. a"));

        // A later snapshot replaces the whole state -- folding is a plain
        // assignment, not a merge.
        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::task_list::TaskListAction::UpdateStatus {
                ids: vec![1],
                status: crate::task_list::TaskStatus::Completed,
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
        assert!(state.task_list.render().contains("Tasks (1/2 done)"));
    }

    fn with_messages(ids: &[&str]) -> AgentState {
        let mut state = AgentActor::initial_state();
        for id in ids {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::MessageComplete {
                    message: Message::user(*id, "x", 0),
                },
            );
        }
        state
    }

    #[test]
    fn state_view_carries_tasks_and_usage() {
        let mut state = with_messages(&["a"]);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage::without_cache(4, 2),
                iterations: 1,
                context_tokens: 4,
            },
        );
        let view = state.state_view();
        assert_eq!(view.usage_total.input_tokens, 4);
        assert_eq!(view.context_tokens, 4);
        assert!(view.tasks.is_empty());
    }

    #[test]
    fn run_complete_accumulates_usage_total() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.usage_total, UsageTotal::default());
        for (input, output) in [(10u32, 5u32), (7, 3)] {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::RunComplete {
                    at_ms: 0,
                    usage: Usage::without_cache(input, output),
                    iterations: 1,
                    context_tokens: input,
                },
            );
        }
        assert_eq!(state.usage_total.input_tokens, 17);
        assert_eq!(state.usage_total.output_tokens, 8);
    }

    #[test]
    fn run_complete_tracks_last_turn_and_context_tokens_separately_from_total() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.last_turn_usage, None);
        assert_eq!(state.context_tokens, 0);

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: Some(15),
                    cache_read_tokens: None,
                },
                iterations: 2,
                context_tokens: 12,
            },
        );
        // A multi-iteration turn: `usage` is the summed cost, `context_tokens`
        // is only the last call's prompt size — the two must stay distinct.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 20);
        assert_eq!(state.context_tokens, 12);
        assert_eq!(state.usage_total.cache_creation_tokens, Some(15));
        assert_eq!(state.usage_total.cache_read_tokens, None);

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                    cache_creation_tokens: None,
                    cache_read_tokens: Some(25),
                },
                iterations: 1,
                context_tokens: 30,
            },
        );
        // `last_turn_usage`/`context_tokens` are overwritten, not accumulated;
        // `usage_total`'s cache fields sum even though only one side reported
        // each field on any given turn.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 30);
        assert_eq!(state.context_tokens, 30);
        assert_eq!(state.usage_total.input_tokens, 50);
        assert_eq!(state.usage_total.cache_creation_tokens, Some(15));
        assert_eq!(state.usage_total.cache_read_tokens, Some(25));
    }

    #[test]
    fn usage_total_combine_sums_two_agents_treating_no_cache_data_as_none() {
        let a = UsageTotal {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: Some(3),
            cache_read_tokens: None,
        };
        let b = UsageTotal {
            input_tokens: 20,
            output_tokens: 8,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        };
        let combined = a.combine(&b);
        assert_eq!(combined.input_tokens, 30);
        assert_eq!(combined.output_tokens, 13);
        assert_eq!(combined.cache_creation_tokens, Some(3));
        assert_eq!(
            combined.cache_read_tokens, None,
            "neither agent ever reported cache reads"
        );
    }

    #[test]
    fn park_sets_parked_and_input_clears_it() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked { at_ms: 0 });
        assert!(state.parked);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("wake"),
            },
        );
        assert!(!state.parked);
    }

    #[test]
    fn cancel_event_removes_selected_timers() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;
        let a = TimerRecord::arm(
            "a".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(1),
            0,
        );
        let b = TimerRecord::arm(
            "b".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(1),
            0,
        );
        let (ia, ib) = (a.id.clone(), b.id.clone());
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: a,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: b,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerCancelled {
                at_ms: 0,
                ids: vec![ia],
            },
        );
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].id, ib);
    }

    #[test]
    fn coarse_event_filters_streaming_noise_and_input() {
        use horsie_models::events::{InputMessageEvent, TextChunkEvent};
        // Streaming noise → None.
        assert!(
            coarse_event(&AgentEvent::TextChunk(TextChunkEvent {
                message_id: "m".into(),
                index: 0,
                text: "noise".into(),
            }))
            .is_none()
        );
        // InputMessage is suppressed from the persistence stream (persisted by the
        // actor instead).
        assert!(
            coarse_event(&AgentEvent::InputMessage(InputMessageEvent {
                message_id: "m".into(),
                input: AgentInput::user_message("m", "hi"),
            }))
            .is_none()
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod retry_tests {
    use super::*;
    use horsie_agentcore::EventSinkError;
    use horsie_agentcore::testkit::{
        CollectingEventSink, FailingEventSink, MockProvider, MockToolbox, Script,
    };
    use horsie_agentcore::{CompletionResponse, EmptyToolbox, LlmError, StopReason, ToolSpec};
    use horsie_models::agent::{TextPart, ToolCallPart, Usage};

    fn text_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn tool_response(id: &str, name: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn echo_toolbox() -> Arc<MockToolbox> {
        MockToolbox::new(
            vec![ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            Arc::new(|_, input| Ok(input)),
        )
    }

    async fn run(
        provider: Arc<MockProvider>,
        toolbox: Arc<dyn Toolbox>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let outcome = run_with_retries(
            provider.clone(),
            toolbox,
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            false,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    #[tokio::test]
    async fn a_transient_error_is_retried_when_nothing_was_journaled() {
        let provider = MockProvider::scripted(Script::of([
            Err(LlmError::Overloaded),
            Ok(text_response("second time lucky")),
        ]));
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 1).await;

        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(calls, 2, "the transient failure should have been retried");
    }

    #[tokio::test]
    async fn a_permanent_error_is_not_retried() {
        // #61 item 21: every AgentError::Provider used to be retried identically,
        // so a 401 or a 400 context-length error burned the whole retry budget.
        let provider = MockProvider::failing(LlmError::ApiError {
            status: 401,
            message: "bad key".into(),
        });
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 3).await;

        assert_eq!(calls, 1, "a permanent error must not be retried");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => assert!(
                !recoverable,
                "a 401 must not be reported to the user as recoverable"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    async fn run_with_sink(
        provider: Arc<MockProvider>,
        sink: Arc<dyn EventSink>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let outcome = run_with_retries(
            provider.clone(),
            Arc::new(EmptyToolbox),
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            false,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    /// #61 item 22, half one: the failure raised *inside* `complete()`.
    ///
    /// A journal write failure surfacing through the provider arrives as
    /// `LlmError::EventSink` → `AgentError::Provider`, which this layer used to
    /// retry against the LLM — burning tokens on a disk fault.
    #[tokio::test]
    async fn a_sink_failure_from_the_provider_is_not_retried_against_the_llm() {
        let provider = MockProvider::scripted(Script::of([]).then_repeating_with(|| {
            Err(LlmError::EventSink(EventSinkError(
                "journal write failed: disk full".into(),
            )))
        }));
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "a journal failure must not be retried against the LLM"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// #61 item 22, half two: the same root cause raised by the agent loop's own
    /// `events.emit(...)?`, which becomes `AgentError::EventSink`.
    ///
    /// The issue's complaint was that one root cause got two different verdicts
    /// depending on where it surfaced. Both paths must agree, and neither may
    /// retry against the LLM.
    #[tokio::test]
    async fn a_sink_failure_at_turn_start_costs_no_tokens() {
        // `Agent::run` journals the input message before it ever calls the
        // provider, so a journal that is already down fails the turn for free.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::always("journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(calls, 0, "the provider must never be reached");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sink_failure_mid_turn_is_not_retried_and_agrees_with_the_provider_path() {
        // Let the input message and the message-start through, so the provider is
        // genuinely engaged before the journal dies — the realistic shape.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::after(2, "journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "the turn must not be re-run against the LLM after a journal failure"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(
                    !recoverable,
                    "both sink-failure paths must report the same verdict"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_transient_error_after_journaled_progress_is_not_retried() {
        // The crux of #61 item 21: the retry rebuilds the turn from the ORIGINAL
        // history, which does not contain the events the failed attempt already
        // persisted. Retrying here would leave a phantom turn in the durable
        // transcript that the model never saw, replayed into every later turn.
        let provider = MockProvider::scripted(Script::of([
            Ok(tool_response("call-1", "echo")),
            Err(LlmError::Overloaded),
            Ok(text_response("must never be reached")),
        ]));
        let (outcome, calls) = run(provider, echo_toolbox(), 3).await;

        assert_eq!(
            calls, 2,
            "once a tool result is journaled the turn must not restart from a \
             history that omits it"
        );
        assert!(
            matches!(outcome, RunOutcome::Failed { .. }),
            "got {outcome:?}"
        );
    }
}

/// The run-id fence: a report can only speak for the run it came from.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod fence_tests {
    use super::*;
    use crate::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{InMemoryJournal, spawn_root};

    struct HangingProvider;
    #[async_trait]
    impl ContextProvider for HangingProvider {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
        }
    }

    struct OutcomeChannel(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
    #[async_trait]
    impl AgentOutcomeSink for OutcomeChannel {
        async fn deliver(&self, outcome: AgentOutcome) {
            let _ = self.0.send(outcome);
        }
    }

    /// A run that was superseded can still be unwinding, and its report must not
    /// be mistaken for the live run's. Taking its word for it would clear the
    /// live run's handle — leaving a turn nobody can stop and a parent told that
    /// a turn it never saw is over.
    #[tokio::test]
    async fn a_report_from_a_superseded_run_is_ignored() {
        let (tx, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingProvider),
            parent: Arc::new(OutcomeChannel(tx)),
            session_id: uuid::Uuid::new_v4(),
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = spawn_root(AgentActor::new(ctx, params), journal);

        // Run 0 starts and hangs in `provide`, so it is genuinely in flight.
        agent
            .tell(AgentCommand::Resume {
                results: Vec::new(),
                message: Some("first".into()),
                subagent_results: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A report from some earlier run arrives late.
        agent
            .tell(AgentCommand::RunFinished(Box::new(RunReport {
                run_id: 99,
                outcome: RunOutcome::Completed {
                    text: "from a run that is over".into(),
                },
            })))
            .await
            .unwrap();

        // Run 0 is still in flight, so a second turn is refused — the fence
        // held. Without it, `running` would have been cleared and this would
        // start a second background loop against the same journal.
        agent
            .tell(AgentCommand::Resume {
                results: Vec::new(),
                message: Some("second".into()),
                subagent_results: Vec::new(),
            })
            .await
            .unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::PageLog {
                before: None,
                max: 50,
                reply,
            })
            .await
            .unwrap();
        let page = rx.await.unwrap();
        assert_eq!(
            page.entries.len(),
            1,
            "the refused turn must journal nothing: {:?}",
            page.entries
        );
        assert!(
            outcomes.try_recv().is_err(),
            "a superseded run's outcome must not reach the parent"
        );
    }
}
