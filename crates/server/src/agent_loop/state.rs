//! An agent's durable state, and the fold that produces it.
//!
//! Everything here is either journaled or derived from what was journaled, so
//! it is a durability contract before it is a data structure: a field that
//! fails to deserialize takes `recover()` down for every existing session. The
//! fold is kept beside the state it folds into, and away from the actor that
//! journals the events, because the two answer different questions — the actor
//! decides *what happened*, and this decides *what that means*.
//!
//! Nothing in the fold may read a clock or generate an id. Replaying a journal
//! has to reproduce exactly what ran live, cursors and all, so every stamp
//! arrives on the event.

use crate::agent_loop::context::AskedQuestion;
use horsie_agentcore::{
    AgentEvent, AgentLogBody, AgentLogEntry, CompactionEntry, ContentPart, LifecycleEvent, Message,
    QueuedLifecycle, Role, TurnBeganLifecycle, Usage,
};
use serde::{Deserialize, Serialize};

/// What a live reader gets for one step forward.
///
/// Entries and deltas are answered together because they are two halves of one
/// position, and separating them would let a client hold a delta that belongs
/// after an entry it has not seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadOutcome {
    /// Set only on a cursorless read: what the replayed window covers, and
    /// whether the cap left anything behind. A resuming caller already knows
    /// where it is, so it gets `None`.
    pub window: Option<ReplayWindow>,
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
    pub cursor: crate::agent_loop::agent_log::Cursor,
}

/// What a cursorless replay covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub has_more_before: bool,
    pub earliest_seq: Option<u64>,
}

impl ReadOutcome {
    /// Nothing new — the reader is exactly where the agent is.
    fn nothing(cursor: crate::agent_loop::agent_log::Cursor) -> Self {
        Self {
            window: None,
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
    /// This agent was seeded from another conversation: `state` is the history
    /// it adopts, `seed` the synthetic message appended after it.
    ///
    /// One event rather than a snapshot written behind the actor's back, so a
    /// fork's own journal explains where its history came from. Only ever the
    /// *first* event an agent has — replacing state wholesale is safe precisely
    /// because nothing has run.
    ///
    /// Boxed: a whole conversation is far larger than any other variant here,
    /// and an enum is as big as its widest arm.
    Seeded {
        state: Box<AgentState>,
        seed: Box<Message>,
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
    /// completed, so there is no `last_turn_usage` to set and no iteration count
    /// worth recording. Exactly one of the two ends a run, so folding both into
    /// `usage_total` cannot double-count.
    RunAborted {
        usage: Usage,
        context_tokens: u32,
        at_ms: u64,
    },
    RunCancelled {
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
    /// run that produced it was holding a `Vec<Message>` in prompt order, which
    /// is not log order; resolving the two is the fold's job because the fold is
    /// the only thing holding the log.
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
    /// A turn began, consuming these queue items — and, if the agent was parked,
    /// answering these questions. One event so a crash anywhere in the window
    /// replays to the same place.
    TurnBegan {
        consumed: Vec<String>,
        /// Every question this turn *answered*. Empty when the turn abandoned
        /// them instead, which is what a plain message does.
        answered: Vec<String>,
        at_ms: u64,
    },
    /// A capability parked the run on this call.
    ///
    /// The actor's own record, distinct from whatever the capability keeps:
    /// being parked governs things no capability can see — whether the queue
    /// may start a turn, and which dangling calls recovery must leave alone.
    /// `note` says what is being waited for, which for `ask_user` is the
    /// question and for anything else is its own words.
    ///
    /// One event per call rather than one per park: a turn may ask several
    /// questions, and each is parked on as its own tool call.
    ParkedOn {
        call: String,
        note: String,
        at_ms: u64,
    },
    /// This agent was equipped with these capabilities.
    ///
    /// Written once, the first time the agent loads, from what its runner
    /// supplied. After that the journal is the source: a capability's config
    /// travels with its folded state, so a reload cannot hand the agent a set
    /// that has forgotten what it was waiting on.
    Equipped {
        capabilities: crate::agent_loop::capabilities::Capabilities,
        at_ms: u64,
    },
    /// One of this agent's capabilities recorded something.
    ///
    /// The event is the capability's own, and the fold hands it to every
    /// capability rather than looking one up by name: a capability that does
    /// not own the arm ignores it, which is the same rule the journal already
    /// uses for a runner's slice. Wrapping rather than flattening keeps
    /// `AgentDomainEvent` a list of things the *actor* does, with one arm for
    /// things its capabilities do.
    Capability(crate::agent_loop::capabilities::CapEvent),
}

/// The conversation history reconstructed by folding [`AgentDomainEvent`]s,
/// plus what the actor itself holds: the queue, the park, and the capabilities
/// this agent was equipped with.
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
    /// Accepted-but-undelivered things addressed to this agent, oldest first.
    ///
    /// The queue lives here rather than on the session because a message is
    /// addressed to an *agent*: once one can name a subagent or a workflow step,
    /// a session-level queue has nowhere to put it. Durable for the same reason
    /// timers are — an accepted message is a promise, and a crash must not
    /// forget it.
    #[serde(default)]
    pub inbox: Vec<crate::agent_loop::Incoming>,
    /// Every question this agent is parked on, oldest first. A turn may ask
    /// several at once, and the run cannot resume until all of them have a
    /// result.
    #[serde(default)]
    pub asks: Vec<crate::agent_loop::AskedQuestion>,
    /// True while the agent has parked itself awaiting a timer (no run in flight).
    #[serde(default)]
    pub parked: bool,
    /// Consecutive turns this agent ended without the result it owed.
    ///
    /// Durable, and reset by any turn that ends properly: it is the budget
    /// behind the nudge, and a process that dies mid-nudge must not hand the
    /// model a fresh one every restart.
    #[serde(default)]
    pub nudges: u32,
    /// True between a turn beginning and that turn reaching a boundary.
    ///
    /// Durable because only a crash can leave one open: every boundary an agent
    /// reaches under its own power journals something, so a fold that still
    /// reads `true` at recovery describes a turn no process is running any
    /// more. That is the whole of how an interruption is detected, and it is
    /// detected *here* because this is the only place the fact exists — an
    /// owner sees a status, which cannot say whose turn produced it.
    ///
    /// "Under its own power" is not quite all of them. A turn that fails
    /// *before* the loop is entered — start hooks that abandon it, a context or
    /// toolbox that will not build — never reaches `Agent::run`, so no
    /// `RunAborted` banks it and this stays set through a failure the owner was
    /// told about directly. The owner reconciles that against the status it
    /// already recorded; see `TurnEnd::Interrupted`.
    #[serde(default)]
    pub turn_in_flight: bool,
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
    /// What this agent can do, and whatever each of those has folded.
    ///
    /// Durable like the task list and the timers, and for the same reason: a
    /// capability's state is a fact about this agent. The one that forced it
    /// here is `ask_user` — the questions an agent is parked on are a pointer
    /// into this transcript, so nothing outside this journal can hold them.
    ///
    /// Round-trips as `Vec<CapSlice>`, which keeps the journal typed while
    /// dispatch stays open; see [`crate::agent_loop::capabilities`].
    #[serde(default)]
    pub capabilities: crate::agent_loop::capabilities::Capabilities,
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

/// The one message a compaction boundary shows the model.
///
/// Two labelled sections rather than one blob, because they have different
/// truth conditions. The summary is the model's own prose and may be wrong at
/// the edges. The carried state is exact and must survive verbatim — a
/// summariser that renders "task 3: in progress" as prose has destroyed the id
/// the agent needs to call `task_list` correctly.
///
/// A `User` message because that is the role every provider accepts as
/// context-setting; there is no wire on which a synthetic assistant turn with
/// no request behind it is safe.
#[must_use]
pub fn boundary_message(entry: &CompactionEntry, at_ms: u64) -> Message {
    let text = format!(
        "This conversation was compacted: earlier history is summarised below \
         rather than shown in full. The messages after this one are verbatim.\n\n\
         ## Summary of earlier work\n{}\n\n## Current state\n{}",
        entry.summary.trim(),
        entry.carried_state.trim(),
    );
    Message {
        // Derived from the boundary's own position, never generated, for the
        // reason `hook:{n}` is: replay must reproduce the id it produced live.
        id: format!("compaction:{}", entry.covers_through_seq),
        role: Role::User,
        parts: vec![ContentPart::Text(horsie_models::agent::TextPart { text })],
        created_at_ms: at_ms,
        started_at_ms: None,
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
    /// This conversation as a fork's starting point.
    ///
    /// Everything that is *about the conversation* carries; everything that is
    /// in flight, or is a bill, does not. A fork that inherited an ask would
    /// park on a question nobody put to it; one that inherited `turn_in_flight`
    /// would be reported interrupted before it had ever run; one that inherited
    /// `usage_total` would make the session's aggregate count the same tokens
    /// twice, once under each conversation.
    ///
    /// Cut at `at_seq` — the branch point, read when the fork was asked for.
    /// Not at the log's current end: journaling the fork writes a `Forked`
    /// entry onto this very log, and a source that is mid-turn goes on
    /// appending while the seed is being built. Copying to the end handed the
    /// fork its own creation marker and whatever else had landed since.
    ///
    /// `next_seq` becomes `at_seq` for the same reason: the fork's own entries
    /// number on from where the copied ones stop, so every cursor into the
    /// copied log still resolves and nothing collides.
    ///
    /// Capabilities do not carry either, and it is the same rule rather than a
    /// new one: a capability's folded state is what this agent has in flight —
    /// the questions it is parked on above all — so inheriting it is inheriting
    /// the ask. A fork is equipped when it loads, from its own runner.
    ///
    /// So a fork starts with an empty task list, which is intended and not an
    /// oversight of the conversion that made the list a capability. It is only
    /// ever visible as an absence, which is why it is written down here.
    #[must_use]
    pub fn scrub_for_fork(&self, at_seq: u64) -> Self {
        Self {
            log: self
                .log
                .iter()
                .filter(|e| e.seq < at_seq)
                .cloned()
                .collect(),
            next_seq: at_seq,
            context_tokens: self.context_tokens,
            inbox: Vec::new(),
            asks: Vec::new(),
            nudges: 0,
            parked: false,
            turn_in_flight: false,
            usage_total: UsageTotal::default(),
            last_turn_usage: None,
            capabilities: crate::agent_loop::capabilities::Capabilities::default(),
        }
    }

    fn push(&mut self, at_ms: u64, body: AgentLogBody) {
        self.log.push(AgentLogEntry {
            seq: self.next_seq,
            at_ms,
            body,
        });
        self.next_seq += 1;
    }

    /// Whether this agent has ever spoken to a provider.
    ///
    /// Not `log.is_empty()`: a queued message and a provisioning stage both
    /// append entries before any run, so an agent with a full log can still be
    /// starting up for the first time — which is what `SessionStart` reports as
    /// `startup` rather than `resume`.
    #[must_use]
    pub fn has_run(&self) -> bool {
        self.log
            .iter()
            .any(|e| matches!(e.body, AgentLogBody::Llm(_)))
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
    /// [`crate::agent_loop::hook_translation::translate`] is where that is decided, in one
    /// exhaustive match, and any future non-model entry inherits the obligation.
    pub fn prompt_messages(&self) -> Vec<Message> {
        // Where the prompt starts. Everything below the newest boundary is
        // represented by that boundary's own message and nothing else — which
        // is the only thing compaction changes about this function.
        let boundary = self.last_boundary();
        let from_seq = boundary.map_or(0, |(_, e)| e.retained_from_seq);

        boundary
            .map(|(at_ms, e)| boundary_message(e, at_ms))
            .into_iter()
            .chain(
                self.log
                    .iter()
                    .filter(|e| e.seq >= from_seq)
                    .filter_map(|e| match &e.body {
                        AgentLogBody::Llm(m) => Some(m.clone()),
                        AgentLogBody::Hook(h) => crate::agent_loop::hook_translation::translate(h),
                        // Every lifecycle variant, present and future. This arm is the
                        // reason `Lifecycle` is one union rather than nine flattened
                        // ones: provider isolation cannot be forgotten for a variant
                        // added later.
                        AgentLogBody::Lifecycle(_) => None,
                        // A boundary reached here is never the newest one — the
                        // newest was lifted out above — so it is history. Its
                        // span is already folded into the summary of every
                        // boundary that followed it, and replaying it would put
                        // the same account in the prompt twice.
                        AgentLogBody::Compaction(_) => None,
                    }),
            )
            .collect()
    }

    /// The newest compaction boundary and the moment it was taken, if this
    /// agent has ever compacted.
    ///
    /// A reverse scan rather than a stored pointer: state is a serialization
    /// contract, and a cached index is a second thing that can disagree with
    /// the log after a partial fold. Boundaries are rare and the scan stops at
    /// the first one.
    #[must_use]
    pub fn last_boundary(&self) -> Option<(u64, &CompactionEntry)> {
        self.log.iter().rev().find_map(|e| match &e.body {
            AgentLogBody::Compaction(c) => Some((e.at_ms, c)),
            AgentLogBody::Llm(_) | AgentLogBody::Hook(_) | AgentLogBody::Lifecycle(_) => None,
        })
    }

    /// Turn "the retained window starts at this message" into log sequence
    /// numbers: `(covers_through_seq, retained_from_seq)`.
    ///
    /// Deterministic under replay because it is a search of an append-only log
    /// from a fold that has already applied everything before this event — the
    /// same log, the same id, the same answer.
    ///
    /// Two cases collapse to "retain nothing": no id at all (a summary-only
    /// compaction), and an id the log does not hold. The second should not
    /// happen — the run built its history from this log — but the honest
    /// failure is to show the model the summary alone rather than to guess a
    /// seq and silently resurrect or drop messages around it.
    #[must_use]
    fn resolve_boundary(&self, retained_from_message_id: Option<&str>) -> (u64, u64) {
        let retain_nothing = (self.tail_seq().unwrap_or(0), self.next_seq);
        let Some(id) = retained_from_message_id else {
            return retain_nothing;
        };
        let Some(idx) = self
            .log
            .iter()
            .position(|e| e.body.id().is_some_and(|got| got == id))
        else {
            tracing::warn!(
                message_id = id,
                "a compaction named a message this log does not hold; \
                 retaining nothing"
            );
            return retain_nothing;
        };
        let retained_from_seq = self.log[idx].seq;
        // The entry immediately before it in the log, read by position rather
        // than as `seq - 1`: the log is contiguous today, and this stays right
        // if it is ever front-trimmed.
        let covers_through_seq = idx
            .checked_sub(1)
            .map_or(retained_from_seq, |prev| self.log[prev].seq);
        (covers_through_seq, retained_from_seq)
    }

    /// The seq of every compaction boundary, oldest first.
    ///
    /// These are the conversation ids: conversation N is the span
    /// `(previous boundary, this boundary]`, so the boundary that closes a
    /// conversation is what names it. A client seeking across compactions pages
    /// on these.
    #[must_use]
    pub fn boundary_seqs(&self) -> Vec<u64> {
        self.log
            .iter()
            .filter(|e| matches!(e.body, AgentLogBody::Compaction(_)))
            .map(|e| e.seq)
            .collect()
    }

    /// The tasks this agent's task list holds, asked of the capability that
    /// owns it.
    ///
    /// A named question rather than a read: `view()` is what a capability says a
    /// client may see, and [`CapView`] is an enum, so the arm *is* the question.
    /// Reading the persisted slice instead would work just as well and would
    /// tie the agent document's shape to the journal's — an API change would
    /// then force a journal migration.
    ///
    /// Empty for an agent equipped without a task list, which is a real state
    /// now that it is a capability rather than a field every agent had whether
    /// or not anything could reach it.
    #[must_use]
    fn tasks(&self) -> Vec<crate::agent_loop::task_list::TaskRecord> {
        self.capabilities
            .views()
            .into_iter()
            .map(|view| match view {
                crate::agent_loop::capabilities::CapView::TaskList(tasks) => tasks,
            })
            .next()
            .unwrap_or_default()
    }

    /// This agent's current values, for the agent document.
    pub fn state_view(&self) -> AgentStateView {
        AgentStateView {
            tasks: self.tasks(),
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
        after: Option<crate::agent_loop::agent_log::Cursor>,
        deltas: &[String],
    ) -> ReadOutcome {
        let tail = self.tail_seq();
        let Some(cursor) = after else {
            // No position at all: the newest window, capped. A long-running
            // session must not resend its whole history on every open, and the
            // caller is told when the cap bit so it can page back for the rest.
            let (entries, truncated) = crate::agent_loop::agent_log::replay_window(&self.log);
            return ReadOutcome {
                window: Some(ReplayWindow {
                    has_more_before: truncated,
                    earliest_seq: entries.first().map(|e| e.seq),
                }),
                entries: entries.to_vec(),
                deltas: deltas.to_vec(),
                reset_deltas: false,
                cursor: crate::agent_loop::agent_log::Cursor {
                    entry_seq: tail.unwrap_or(0),
                    delta_seq: deltas.len(),
                },
            };
        };

        let entries =
            crate::agent_loop::agent_log::page_after(&self.log, cursor.entry_seq).to_vec();
        if !entries.is_empty() {
            // Behind the tail. The deltas belong after the entries this reader
            // is only now receiving, so they wait for the next step.
            return ReadOutcome {
                window: None,
                cursor: crate::agent_loop::agent_log::Cursor {
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
                window: None,
                entries: Vec::new(),
                deltas: deltas.to_vec(),
                reset_deltas: true,
                cursor: crate::agent_loop::agent_log::Cursor {
                    entry_seq: cursor.entry_seq,
                    delta_seq: deltas.len(),
                },
            };
        }

        if cursor.delta_seq == deltas.len() {
            return ReadOutcome::nothing(cursor);
        }

        ReadOutcome {
            window: None,
            entries: Vec::new(),
            deltas: deltas[cursor.delta_seq..].to_vec(),
            reset_deltas: false,
            cursor: crate::agent_loop::agent_log::Cursor {
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

impl AgentState {
    /// Fold one event into this state.
    ///
    /// The whole of what a journal replay does, and the reason every field
    /// above is a durability contract: run this over an agent's events in order
    /// and the transcript, the queue, the timers and the totals come back
    /// exactly as they were. Nothing here may read a clock or generate an id —
    /// every stamp arrives on the event.
    pub(crate) fn apply(self, event: AgentDomainEvent) -> Self {
        let mut state = self;
        match event {
            AgentDomainEvent::Seeded {
                state: seeded,
                seed,
            } => {
                // Wholesale, because this is the agent's first event: anything
                // already here would be a bug rather than a history to merge.
                state = *seeded;
                let at_ms = seed.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(*seed));
            }
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::MessageComplete { message }
            | AgentDomainEvent::MessageAborted { message } => {
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
            AgentDomainEvent::Compacted {
                summary,
                carried_state,
                retained_from_message_id,
                trigger,
                instructions,
                tokens_before,
                tokens_after,
                at_ms,
            } => {
                let (covers_through_seq, retained_from_seq) =
                    state.resolve_boundary(retained_from_message_id.as_deref());
                let entry = CompactionEntry {
                    summary,
                    carried_state,
                    covers_through_seq,
                    retained_from_seq,
                    trigger,
                    instructions,
                    tokens_before,
                    tokens_after,
                };
                // `context_tokens` is what the next auto-compaction check
                // reads, and the whole point of a compaction is that this
                // number just dropped. Leaving it at the pre-compaction size
                // would make the very next turn compact again immediately.
                state.context_tokens = tokens_after;
                state.push(at_ms, AgentLogBody::Compaction(entry));
            }
            AgentDomainEvent::Received { item, at_ms } => {
                // Only a person's message becomes a visible queue entry. A
                // report and a timer are already narrated elsewhere — the
                // session records a subagent's news on this very log, and a
                // wake becomes the turn's own input message — so surfacing
                // them here would render the same fact twice.
                if let crate::agent_loop::Incoming::User { id, text } = &item {
                    state.push(
                        at_ms,
                        AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(QueuedLifecycle {
                            id: id.clone(),
                            text: text.clone(),
                        })),
                    );
                }
                state.inbox.push(item);
            }
            AgentDomainEvent::TurnBegan {
                consumed,
                answered,
                at_ms,
            } => {
                // The entry names only what a client is tracking — the queued
                // messages it is showing as unread. Reports and wakes were never
                // shown as queued, so crossing them off would name ids nothing
                // holds.
                let visible = state
                    .inbox
                    .iter()
                    .filter(|i| i.is_user() && consumed.iter().any(|id| id == i.id()))
                    .map(|i| i.id().to_string())
                    .collect();
                state.push(
                    at_ms,
                    AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(TurnBeganLifecycle {
                        consumed: visible,
                        answered: answered.clone(),
                    })),
                );
                state
                    .inbox
                    .retain(|i| !consumed.iter().any(|id| id == i.id()));
                // A turn beginning ends the park either way: the questions were
                // answered, or the user moved on and they were abandoned. Both
                // record a result for every call before the turn starts.
                state.asks.clear();
                state.turn_in_flight = true;
            }
            AgentDomainEvent::ParkedOn { call, note, .. } => {
                state.asks.push(AskedQuestion {
                    tool_call_id: Some(call),
                    question: note,
                });
                // Parking is a turn boundary: the run is over, and whatever the
                // capability is waiting for starts the next one.
                state.turn_in_flight = false;
            }
            AgentDomainEvent::Equipped { capabilities, .. } => state.capabilities = capabilities,
            AgentDomainEvent::Capability(event) => state.capabilities.apply(&event),
            AgentDomainEvent::Parked { .. } => {
                state.parked = true;
                state.turn_in_flight = false;
                // Parking is a turn ending properly: the budget is for turns
                // that end with nothing to wake them.
                state.nudges = 0;
            }
            AgentDomainEvent::Nudged { .. } => {
                state.nudges = state.nudges.saturating_add(1);
                state.turn_in_flight = false;
            }
            AgentDomainEvent::RunComplete {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(usage);
                state.turn_in_flight = false;
            }
            AgentDomainEvent::RunAborted {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.turn_in_flight = false;
            }
            AgentDomainEvent::RunCancelled { .. } => state.turn_in_flight = false,
        }
        state
    }
}

/// Whether folding this event appends a log entry — i.e. consumes a `seq`.
///
/// Kept beside [`AgentState::apply`] deliberately: the two must agree, and
/// a variant added to one without the other would either strand deltas under an
/// entry that superseded them or clear them for an event that appended nothing.
pub(super) fn coarse_appends_an_entry(e: &AgentDomainEvent) -> bool {
    matches!(
        e,
        AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
    )
}

/// Map a single streaming event to the coarse domain event that should be
/// persisted, or `None` for streaming noise and for `InputMessage` (see
/// [`PersistSink`]).
pub(super) fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
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
        AgentEvent::RunAborted(ev) => Some(AgentDomainEvent::RunAborted {
            usage: ev.usage.clone(),
            context_tokens: ev.context_tokens,
            at_ms: ev.at_ms,
        }),
        AgentEvent::Compacted(ev) => Some(AgentDomainEvent::Compacted {
            summary: ev.summary.clone(),
            carried_state: ev.carried_state.clone(),
            retained_from_message_id: ev.retained_from_message_id.clone(),
            trigger: ev.trigger.clone(),
            instructions: ev.instructions.clone(),
            tokens_before: ev.tokens_before,
            tokens_after: ev.tokens_after,
            at_ms: ev.at_ms,
        }),
        // A lifecycle entry rather than a `Compaction` one: nothing moved, so
        // nothing may look like a boundary. `prompt_messages` drops every
        // lifecycle body, so the notice answers the person and never reaches
        // the model — which is right, since the model was not asked anything.
        AgentEvent::CompactionSkipped(ev) => Some(AgentDomainEvent::LifecycleRecorded {
            event: LifecycleEvent::CompactionSkipped(ev.detail.clone()),
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::agent_loop::agent_actor::AgentActor;
    use horsie_actor::EventSourcedActor;
    use horsie_agentcore::AgentInput;
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

    // --- Compaction boundaries ---------------------------------------------
    //
    // The whole of the compaction contract as seen from state: where a prompt
    // starts, and what a boundary that is no longer the newest one means.

    /// A `Compacted` event whose retained window starts at `retained_from`
    /// (a message id), or which retains nothing when that is `None`.
    fn compacted(retained_from: Option<&str>, summary: &str) -> AgentDomainEvent {
        AgentDomainEvent::Compacted {
            summary: summary.into(),
            carried_state: "No tasks.".into(),
            retained_from_message_id: retained_from.map(Into::into),
            trigger: horsie_agentcore::CompactionTrigger::Auto(horsie_agentcore::EmptyOutcome {}),
            instructions: None,
            tokens_before: 1_000,
            tokens_after: 100,
            at_ms: 500,
        }
    }

    /// Builds a state holding `n` user messages at seqs `0..n`.
    fn state_with_messages(n: u64) -> AgentState {
        let mut state = AgentActor::initial_state();
        for i in 0..n {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::InputMessage {
                    message: Message {
                        id: format!("m{i}"),
                        ..user_msg(&format!("message {i}"))
                    },
                },
            );
        }
        state
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[test]
    fn a_log_with_no_boundary_prompts_exactly_as_before() {
        let state = state_with_messages(3);
        assert_eq!(
            texts(&state.prompt_messages()),
            vec!["message 0", "message 1", "message 2"],
            "adding the arm must not change a log that has never compacted"
        );
    }

    #[test]
    fn a_boundary_replaces_everything_it_covers_with_one_message() {
        let mut state = state_with_messages(4);
        // Retains from message 3, so seqs 0..=2 are covered.
        state = AgentActor::apply_event(
            state,
            compacted(Some("m3"), "they discussed the first three things"),
        );

        let prompt = texts(&state.prompt_messages());
        assert_eq!(
            prompt.len(),
            2,
            "one synthetic message, then the retained one"
        );
        assert!(
            prompt[0].contains("they discussed the first three things"),
            "the summary leads the prompt, got {:?}",
            prompt[0]
        );
        assert!(
            prompt[0].contains("No tasks."),
            "carried state rides in the same synthetic message"
        );
        assert_eq!(prompt[1], "message 3");
    }

    #[test]
    fn entries_retained_across_a_boundary_are_sent_raw() {
        let mut state = state_with_messages(4);
        // Retains from message 2 — the summary also covered it, which is the
        // overlap a recency window creates.
        state = AgentActor::apply_event(state, compacted(Some("m2"), "summary"));

        let prompt = texts(&state.prompt_messages());
        assert_eq!(
            prompt[1..],
            ["message 2", "message 3"],
            "a message the summary also covers is still sent verbatim when retained"
        );
    }

    #[test]
    fn only_the_newest_of_two_boundaries_is_honoured() {
        let mut state = state_with_messages(3);
        state = AgentActor::apply_event(state, compacted(Some("m2"), "the first summary"));
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: Message {
                    id: "m9".into(),
                    ..user_msg("message 9")
                },
            },
        );
        // Retains nothing at all, so not even `m9` survives.
        state = AgentActor::apply_event(state, compacted(None, "the second summary"));

        let prompt = texts(&state.prompt_messages());
        assert_eq!(prompt.len(), 1, "nothing survives past the newest boundary");
        assert!(prompt[0].contains("the second summary"));
        assert!(
            !prompt[0].contains("the first summary"),
            "a superseded boundary is history; its span is already folded into \
             the summary that replaced it, so replaying it says the same thing \
             twice"
        );
    }

    #[test]
    fn a_superseded_boundary_translates_to_nothing() {
        let mut state = state_with_messages(2);
        state = AgentActor::apply_event(state, compacted(Some("m1"), "old"));
        // Retains from message 0, pulling the whole log — including the older
        // boundary at seq 2 — back inside the window. That is the case which
        // proves the older boundary is skipped on its own merits rather than by
        // falling outside the range.
        state = AgentActor::apply_event(state, compacted(Some("m0"), "new"));

        let prompt = texts(&state.prompt_messages());
        assert!(
            !prompt.iter().any(|t| t.contains("old")),
            "an older boundary inside the retained window still shows nothing, \
             got {prompt:?}"
        );
    }

    /// The reason the event carries a message id rather than the index the run
    /// computed. A lifecycle entry occupies a log seq but produces no prompt
    /// message, so after even one of them the nth message is not the nth entry.
    /// Sending the index would silently cut the prompt in the wrong place, and
    /// nothing downstream could tell.
    #[test]
    fn a_boundary_resolves_against_log_seqs_not_prompt_positions() {
        let mut state = AgentActor::initial_state();
        // seq 0: a lifecycle entry — invisible to the prompt.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::LifecycleRecorded {
                event: LifecycleEvent::Preparing(horsie_agentcore::PreparingLifecycle {
                    stage: "scanning_workspace".into(),
                    detail: None,
                }),
                at_ms: 1,
            },
        );
        // seqs 1, 2: two messages. `m1` is prompt position 1, log seq 2.
        for i in 0..2u64 {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::InputMessage {
                    message: Message {
                        id: format!("m{i}"),
                        ..user_msg(&format!("message {i}"))
                    },
                },
            );
        }

        state = AgentActor::apply_event(state, compacted(Some("m1"), "summary"));

        let (covers, retained) = match &state.log.last().unwrap().body {
            AgentLogBody::Compaction(c) => (c.covers_through_seq, c.retained_from_seq),
            other => panic!("expected a boundary, got {other:?}"),
        };
        assert_eq!(
            retained, 2,
            "`m1` sits at log seq 2, not at its prompt position of 1"
        );
        assert_eq!(covers, 1);
        assert_eq!(
            texts(&state.prompt_messages())[1],
            "message 1",
            "and the prompt therefore retains exactly the message that was named"
        );
    }

    /// Without this the boundary that just shrank the context leaves the old
    /// size in state, and the next iteration compacts again — every iteration,
    /// forever, each one costing a provider call.
    /// `AgentState` is a serialization contract, and a boundary is the newest
    /// thing in it. A snapshot that lost one would silently un-compact every
    /// recovered session — the prompt would jump back to the whole log, which
    /// is the failure mode that took the supervisor down on 2026-08-02, only
    /// quieter: it would cost money rather than crash.
    #[test]
    fn a_boundary_survives_a_snapshot_round_trip() {
        let mut state = state_with_messages(3);
        state = AgentActor::apply_event(state, compacted(Some("m2"), "what came before"));

        let json = serde_json::to_string(&state).unwrap();
        let back: AgentState = serde_json::from_str(&json).unwrap();

        assert_eq!(back.boundary_seqs(), state.boundary_seqs());
        assert_eq!(
            texts(&back.prompt_messages()),
            texts(&state.prompt_messages()),
            "a recovered agent must prompt from exactly the boundary the live \
             one did"
        );
        let (_, entry) = back.last_boundary().expect("the boundary survived");
        assert_eq!(entry.summary, "what came before");
        assert_eq!(entry.carried_state, "No tasks.");
        assert!(matches!(
            entry.trigger,
            horsie_agentcore::CompactionTrigger::Auto(_)
        ));
    }

    /// The compatibility half: a snapshot written before compaction existed
    /// has no `Compaction` entries and must recover to exactly what it always
    /// did, rather than failing `recover()` for every existing session.
    #[test]
    fn a_snapshot_that_predates_compaction_still_recovers() {
        let state = state_with_messages(2);
        let json = serde_json::to_string(&state).unwrap();
        let back: AgentState = serde_json::from_str(&json).unwrap();
        assert!(back.boundary_seqs().is_empty());
        assert_eq!(
            texts(&back.prompt_messages()),
            vec!["message 0", "message 1"]
        );
    }

    #[test]
    fn a_boundary_resets_the_context_size_it_reports() {
        let mut state = state_with_messages(2);
        state.context_tokens = 9_000;
        state = AgentActor::apply_event(state, compacted(Some("m1"), "summary"));
        assert_eq!(state.context_tokens, 100);
    }

    #[test]
    fn a_compaction_that_retains_nothing_shows_only_the_summary() {
        let mut state = state_with_messages(3);
        state = AgentActor::apply_event(state, compacted(None, "everything, summarised"));
        let prompt = texts(&state.prompt_messages());
        assert_eq!(prompt.len(), 1);
        assert!(prompt[0].contains("everything, summarised"));
    }

    #[test]
    fn boundary_seqs_name_every_conversation() {
        let mut state = state_with_messages(2);
        state = AgentActor::apply_event(state, compacted(Some("m1"), "first"));
        state = AgentActor::apply_event(state, compacted(Some("m1"), "second"));
        assert_eq!(
            state.boundary_seqs(),
            vec![2, 3],
            "a conversation's id is the seq of the boundary that closes it"
        );
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

        let tail = crate::agent_loop::agent_log::page_before(&state.log, None, 2);
        assert_eq!(
            tail.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // The cursor resolves against a hook entry exactly like a message.
        let forward = crate::agent_loop::agent_log::page_after(&state.log, 1);
        assert_eq!(forward.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

        let back = crate::agent_loop::agent_log::page_before(&state.log, Some(1), 10);
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
            Some(crate::agent_loop::agent_log::Cursor {
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
            Some(crate::agent_loop::agent_log::Cursor {
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
            Some(crate::agent_loop::agent_log::Cursor {
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
            Some(crate::agent_loop::agent_log::Cursor {
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

    /// The agent document's task list is served over HTTP and drawn by the web
    /// UI, and once the list belongs to a capability the only honest way to
    /// read it is to ask that capability — `view()`, typed through `CapView`. A
    /// shadow copy kept on `AgentState` by the fold would be the leak this
    /// whole design removes, and it would go stale the first time a capability
    /// was equipped without one.
    #[test]
    fn the_state_view_reads_the_task_list_back_out_of_the_capability() {
        use crate::agent_loop::capabilities::{CapEvent, Capabilities, Capability, task_list};
        let mut state = AgentActor::initial_state();
        assert!(
            state.state_view().tasks.is_empty(),
            "an agent with no task list capability shows none"
        );

        let mut list = crate::agent_loop::task_list::TaskListState::default();
        list.apply(crate::agent_loop::task_list::TaskListAction::Create {
            tasks: vec!["a".to_string(), "b".to_string()],
        })
        .unwrap();
        state.capabilities = Capabilities::new(vec![Capability::TaskList(
            task_list::TaskListCapability::new(),
        )]);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::Capability(CapEvent::TaskList(task_list::Event::Changed {
                snapshot: list,
            })),
        );

        let view = state.state_view();
        assert_eq!(view.tasks.len(), 2);
        assert_eq!(view.tasks[0].content, "a");
        assert_eq!(view.tasks[1].id, 2);
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

    /// A run that was cancelled or failed still spent what it spent. It used to
    /// bank nothing at all: `usage_total` only advanced on `RunComplete`, which
    /// an aborted run never emits, so an interrupted workflow step reported
    /// `0 tokens` after burning provider turns.
    #[test]
    fn an_aborted_run_banks_what_it_spent() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage::without_cache(10, 5),
                iterations: 1,
                context_tokens: 10,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunAborted {
                at_ms: 1,
                usage: Usage::without_cache(7, 3),
                context_tokens: 7,
            },
        );
        assert_eq!(state.usage_total.input_tokens, 17);
        assert_eq!(state.usage_total.output_tokens, 8);
        assert_eq!(state.context_tokens, 7);
        // No turn completed, so the last *completed* turn is still the first
        // one — an aborted run has no turn usage to report.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 10);
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
