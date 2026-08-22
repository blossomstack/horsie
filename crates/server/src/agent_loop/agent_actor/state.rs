//! The transcript and the running totals: what folding an agent's events
//! leaves behind.
//!
//! [`AgentState`] is a durability contract — it is snapshotted, so a field that
//! fails to deserialize takes down `recover()` for every existing session. Add
//! optional fields; never rename or repurpose one.

use super::*;
use horsie_agentcore::{AgentLogBody, AgentLogEntry, Usage};
use serde::{Deserialize, Serialize};

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
    /// Active timers — durable so they re-arm on recovery and back `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::agent_loop::timers::TimerRecord>,
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
    /// The agent's task list — durable so it survives an actor restart exactly
    /// like timers do; see `crate::agent_loop::task_list`.
    #[serde(default)]
    pub task_list: crate::agent_loop::task_list::TaskListState,
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
    pub(super) fn add(&mut self, usage: &Usage) {
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
pub(super) fn add_optional(total: Option<u64>, delta: Option<u32>) -> Option<u64> {
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
pub(super) fn combine_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
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

pub(super) fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
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

    pub(super) fn push(&mut self, at_ms: u64, body: AgentLogBody) {
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
