//! The transcript, and every component's own durable state.
//!
//! [`AgentState`] holds two things and no third. The [`Transcript`] is shared
//! by construction — it is the one ordered thing a client reads, and nearly
//! every component appends to it. Everything else belongs to exactly one
//! component and lives behind [`ComponentState`], a list rather than a set of
//! named fields so that adding a component adds an entry rather than editing
//! this file.
//!
//! A component reaches its own state with [`AgentState::part`], which is typed:
//! there is no downcast that can fail. It reaches nobody else's *fields* at
//! all — each state's fields are private to the file that owns them, so the
//! compiler, not a convention, is what stops one component reading another's
//! internals. What others may know is whatever methods that file chooses to
//! offer, and [`AgentState`]'s own accessors are those methods forwarded.
//!
//! This is a durability contract: it is snapshotted, and the parts are tagged
//! by `kind`. A tag this build does not know is skipped with a warning rather
//! than failing the load, so removing a component cannot make an old snapshot
//! unreadable.

use crate::agent_loop::prelude::*;
use horsie_agentcore::{AgentLogBody, AgentLogEntry, Usage};
use serde::{Deserialize, Serialize};

/// The transcript and the component states folding an agent's events leaves
/// behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// The sole chronological durable account. The transcript below is a
    /// deterministic person-facing projection of these records.
    history: Vec<crate::agent_loop::events::AgentHistoryEntry>,
    next_history_seq: u64,
    usage_total: UsageTotal,
    last_step_usage: Option<Usage>,
    context_tokens: u32,
    /// Everything the user sees, whether or not the model saw it. Shared: a
    /// timer, a hook, a tool result and a task-list change all land here, in
    /// one order.
    pub(crate) transcript: Transcript,
    /// One entry per component that has any durable state of its own.
    #[serde(deserialize_with = "known_parts")]
    parts: Vec<ComponentState>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            history: Vec::new(),
            next_history_seq: 0,
            usage_total: UsageTotal::default(),
            last_step_usage: None,
            context_tokens: 0,
            transcript: Transcript::default(),
            parts: crate::agent_loop::components::default_parts(),
        }
    }
}

/// Skip parts this build has no component for, rather than failing the load.
///
/// A snapshot outlives the code that wrote it. A removed component leaves its
/// tag behind in every snapshot ever taken, and refusing to load them would
/// make deleting a component a migration; skipping makes it a deletion. The
/// warning is what stops that being silent.
fn known_parts<'de, D>(deserializer: D) -> Result<Vec<ComponentState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(part) => Some(part),
            Err(e) => {
                tracing::warn!(error = %e, "skipping a component state this build cannot read");
                None
            }
        })
        .collect())
}

/// The one thing every component writes to: the ordered log, and the next
/// sequence number to hand out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Transcript {
    /// Renamed twice in its life — from `messages: Vec<Message>` when the
    /// element type became a union, and from `history: Vec<HistoryEntry>` when
    /// entries gained a sequence number — because serde ignores a now-unknown
    /// key, so a rename degrades to an empty transcript rather than failing
    /// `recover()` for every session.
    log: Vec<AgentLogEntry>,
    /// Deterministic across replay for the same reason `hook:{n}` is: the fold
    /// is deterministic, so re-running it produces the same numbers. Held here
    /// rather than derived from `log.len()` so front-trimming stays possible
    /// without renumbering.
    next_seq: u64,
}

impl Transcript {
    /// Every entry, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[AgentLogEntry] {
        &self.log
    }

    /// The next sequence number this transcript will hand out.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The seq of the newest entry, or `None` for an empty transcript. The
    /// tail a cursor is compared against.
    #[must_use]
    pub fn tail_seq(&self) -> Option<u64> {
        self.log.last().map(|e| e.seq)
    }

    /// Append `body` at the next sequence number.
    ///
    /// The single place a `seq` is handed out, so the fold cannot produce a gap
    /// or a duplicate by accident.
    pub(crate) fn push(&mut self, at_ms: u64, body: AgentLogBody) {
        self.log.push(AgentLogEntry {
            seq: self.next_seq,
            at_ms,
            body,
        });
        self.next_seq += 1;
    }

    /// Everything below `at_seq`, renumbering nothing — a sub session's
    /// starting point. See [`AgentState::snapshot_at`].
    fn cut(&self, at_seq: u64) -> Self {
        Self {
            log: self
                .log
                .iter()
                .filter(|e| e.seq < at_seq)
                .cloned()
                .collect(),
            next_seq: at_seq,
        }
    }
}

impl AgentState {
    /// This component's own state, or `None` if it has never had any.
    ///
    /// Typed by the caller: `state.part::<QueueState>()`. No downcast, and no
    /// way to name a part that does not exist.
    #[must_use]
    pub(crate) fn part<T: Part>(&self) -> Option<&T> {
        T::get(&self.parts)
    }

    /// This component's own state, created empty the first time it is asked
    /// for. `None` is unreachable — the part is inserted just above — and the
    /// callers treat it as "nothing to do" rather than panicking, because a
    /// fold must never take the process down.
    pub(crate) fn part_mut<T: Part>(&mut self) -> Option<&mut T> {
        T::get_mut(&mut self.parts)
    }

    /// This session as a sub session's starting point.
    ///
    /// Everything that is *about the session* carries; everything that is in
    /// flight, or is a bill, does not — and each part says which it is, so a
    /// component added later cannot be forgotten here.
    ///
    /// Cut at `at_seq` — the branch point, read when the sub session was asked
    /// for. Not at the log's current end: journaling the sub session writes a
    /// `Branched` entry onto this very log, and a source that is mid-turn goes
    /// on appending while the seed is being built.
    #[must_use]
    pub fn snapshot_at(&self, at_seq: u64) -> Self {
        Self {
            history: self
                .history
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.record,
                        AgentDomainEvent::SystemPromptRecorded { .. }
                            | AgentDomainEvent::AgentInitialized { .. }
                    )
                })
                .cloned()
                .collect(),
            next_history_seq: self.next_history_seq,
            usage_total: UsageTotal::default(),
            last_step_usage: None,
            context_tokens: self.context_tokens,
            transcript: self.transcript.cut(at_seq),
            parts: self
                .parts
                .iter()
                .filter_map(ComponentState::carried)
                .collect(),
        }
    }
}

/// Running token totals held in [`AgentState`]. Distinct from the per-turn
/// wire [`Usage`] (`u32`): this accumulates across all turns, so it is `u64`
/// and owns a `Default`, which the fluorite-generated `Usage` does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotal {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

impl UsageTotal {
    pub(crate) fn add(&mut self, usage: &Usage) {
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
pub(crate) fn add_optional(total: Option<u64>, delta: Option<u32>) -> Option<u64> {
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
pub(crate) fn combine_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
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

pub(crate) fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl AgentState {
    /// How many hook entries this transcript already holds. The next one's
    /// `seq`.
    #[must_use]
    pub fn hook_entry_count(&self) -> usize {
        self.transcript
            .entries()
            .iter()
            .filter(|e| matches!(e.body, AgentLogBody::Hook(_)))
            .count()
    }

    /// Every part's reason the agent must not act yet, in registry order.
    ///
    /// The parts are asked; nothing here knows which of them has an opinion.
    pub(crate) fn vetoes(&self) -> impl Iterator<Item = Blocked> + '_ {
        self.parts.iter().filter_map(|part| part.blocks(self))
    }

    /// Every durable record, including control records hidden from the
    /// person-facing transcript.
    #[must_use]
    pub fn history(&self) -> &[crate::agent_loop::events::AgentHistoryEntry] {
        &self.history
    }

    #[must_use]
    pub fn next_history_seq(&self) -> u64 {
        self.next_history_seq
    }

    pub(crate) fn record_history(&mut self, record: AgentDomainEvent) {
        self.history
            .push(crate::agent_loop::events::AgentHistoryEntry {
                seq: self.next_history_seq,
                record,
            });
        self.next_history_seq = self.next_history_seq.saturating_add(1);
    }

    #[must_use]
    pub fn initialized(&self) -> bool {
        self.history
            .iter()
            .any(|entry| matches!(&entry.record, AgentDomainEvent::AgentInitialized { .. }))
    }

    #[must_use]
    pub fn context_manifest(&self) -> Option<&ContextManifest> {
        self.history.iter().rev().find_map(|entry| {
            if let AgentDomainEvent::AgentInitialized { manifest } = &entry.record {
                Some(manifest)
            } else {
                None
            }
        })
    }

    #[must_use]
    pub fn system_prompt(&self) -> String {
        self.history
            .iter()
            .filter_map(|entry| {
                if let AgentDomainEvent::SystemPromptRecorded { content, .. } = &entry.record {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The open top marker after the newest run boundary.
    #[must_use]
    pub fn open_step(&self) -> Option<(u64, &StepKind)> {
        let from = self
            .history
            .iter()
            .rposition(|entry| matches!(&entry.record, AgentDomainEvent::RunEnded { .. }))
            .map_or(0, |position| position + 1);
        self.history[from..].iter().rev().find_map(|entry| {
            if let AgentDomainEvent::StepStarted { kind } = &entry.record {
                Some((entry.seq, kind))
            } else {
                None
            }
        })
    }

    #[must_use]
    pub fn stop_continuations(&self) -> usize {
        let from = self
            .history
            .iter()
            .rposition(|entry| matches!(&entry.record, AgentDomainEvent::RunEnded { .. }))
            .map_or(0, |position| position + 1);
        self.history[from..]
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.record,
                    AgentDomainEvent::StopHookCompleted {
                        outcome: StopHookOutcome::Continue { .. }
                    }
                )
            })
            .count()
    }

    #[must_use]
    pub fn open_step_has_response(&self) -> bool {
        let Some((marker, _)) = self.open_step() else {
            return false;
        };
        self.history.iter().any(|entry| {
            entry.seq > marker
                && matches!(
                    &entry.record,
                    AgentDomainEvent::MessageComplete { .. }
                        | AgentDomainEvent::MessageAborted { .. }
                        | AgentDomainEvent::StepFailed { .. }
                        | AgentDomainEvent::StopHookCompleted { .. }
                )
        })
    }

    #[must_use]
    pub fn stop_candidate(&self) -> serde_json::Value {
        let Some(message) = self.transcript.entries().iter().rev().find_map(|entry| {
            let AgentLogBody::Llm(message) = &entry.body else {
                return None;
            };
            (message.role == horsie_agentcore::Role::Assistant).then_some(message)
        }) else {
            return serde_json::Value::Null;
        };
        message
            .parts
            .iter()
            .find_map(|part| {
                if let horsie_agentcore::ContentPart::ToolCall(call) = part
                    && call.name == crate::sessions::workflow::SUBMIT_RESULT_TOOL
                {
                    Some(call.input.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                serde_json::Value::String(horsie_agentcore::extract_text(&message.parts))
            })
    }

    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        self.transcript.entries().iter().rev().find_map(|entry| {
            let AgentLogBody::Llm(message) = &entry.body else {
                return None;
            };
            (message.role == horsie_agentcore::Role::Assistant)
                .then(|| horsie_agentcore::extract_text(&message.parts))
        })
    }

    /// Every entry, oldest first.
    #[must_use]
    pub fn log(&self) -> &[AgentLogEntry] {
        self.transcript.entries()
    }

    /// The next sequence number this agent will hand out.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.transcript.next_seq()
    }

    /// The seq of the newest entry, or `None` for an empty log.
    #[must_use]
    pub fn tail_seq(&self) -> Option<u64> {
        self.transcript.tail_seq()
    }

    pub(crate) fn push(&mut self, at_ms: u64, body: AgentLogBody) {
        self.transcript.push(at_ms, body);
    }

    /// Whether this agent has ever spoken to a provider.
    ///
    /// Not `log.is_empty()`: a queued message and a provisioning stage both
    /// append entries before any run, so an agent with a full log can still be
    /// starting up for the first time — which is what `SessionStart` reports as
    /// `startup` rather than `resume`.
    #[must_use]
    pub fn has_run(&self) -> bool {
        self.transcript
            .entries()
            .iter()
            .any(|e| matches!(e.body, AgentLogBody::Llm(_)))
    }

    /// Tool calls the model has made that have no result yet, and that this
    /// agent is not parked on.
    ///
    /// Derived, never stored: the log already holds both halves of every call,
    /// so a second index could only ever disagree with it. Empty means the
    /// agent owes the provider nothing but its next call.
    ///
    /// A parked call is exempt. The agent is *waiting* on that one, on purpose,
    /// and the answer arrives later as an ordinary result; treating it as
    /// outstanding would freeze a parked agent for ever.
    ///
    /// The scan starts at the newest assistant message, not at the beginning:
    /// a new assistant message only ever lands once the previous one's calls
    /// are all answered, so nothing before it can still be open.
    #[must_use]
    pub fn open_tool_calls(&self) -> Vec<String> {
        let entries = self.transcript.entries();
        let from = entries
            .iter()
            .rposition(|e| match &e.body {
                AgentLogBody::Llm(m) => m
                    .parts
                    .iter()
                    .any(|p| matches!(p, horsie_agentcore::ContentPart::ToolCall(_))),
                AgentLogBody::Hook(_)
                | AgentLogBody::Lifecycle(_)
                | AgentLogBody::Compaction(_) => false,
            })
            .unwrap_or(entries.len());
        let mut open: Vec<String> = Vec::new();
        for entry in entries.iter().skip(from) {
            let AgentLogBody::Llm(message) = &entry.body else {
                continue;
            };
            for part in &message.parts {
                match part {
                    horsie_agentcore::ContentPart::ToolCall(c) => open.push(c.id.clone()),
                    horsie_agentcore::ContentPart::ToolResult(r) => {
                        open.retain(|id| *id != r.tool_call_id);
                    }
                    horsie_agentcore::ContentPart::Text(_)
                    | horsie_agentcore::ContentPart::Thinking(_)
                    | horsie_agentcore::ContentPart::SubAgentResult(_)
                    | horsie_agentcore::ContentPart::Artifact(_) => {}
                }
            }
        }
        let parked = self.asks();
        open.retain(|id| {
            !parked
                .iter()
                .any(|a| a.tool_call_id.as_deref() == Some(id.as_str()))
        });
        open
    }

    /// The name and input of the tool call `id`, read off the transcript —
    /// what the actor needs to *run* an open call, scanned the same way
    /// [`Self::open_tool_calls`] found it.
    #[must_use]
    pub fn tool_call_named(&self, id: &str) -> Option<(String, serde_json::Value)> {
        self.transcript.entries().iter().rev().find_map(|e| {
            let AgentLogBody::Llm(message) = &e.body else {
                return None;
            };
            message.parts.iter().find_map(|part| match part {
                horsie_agentcore::ContentPart::ToolCall(c) if c.id == id => {
                    Some((c.name.clone(), c.input.clone()))
                }
                horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::Text(_)
                | horsie_agentcore::ContentPart::Thinking(_)
                | horsie_agentcore::ContentPart::ToolResult(_)
                | horsie_agentcore::ContentPart::SubAgentResult(_)
                | horsie_agentcore::ContentPart::Artifact(_) => None,
            })
        })
    }

    /// This agent's current values, for the agent document.
    #[must_use]
    pub fn state_view(&self) -> AgentStateView {
        AgentStateView {
            tasks: self.task_list().tasks().to_vec(),
            usage_total: self.usage_total(),
            last_turn_usage: self.last_turn_usage().cloned(),
            context_tokens: self.context_tokens(),
            as_of_seq: self.tail_seq().unwrap_or(0),
        }
    }

    /// This agent's usage and context size, without its transcript.
    #[must_use]
    pub fn usage_snapshot(&self) -> AgentUsageSnapshot {
        AgentUsageSnapshot {
            usage_total: self.usage_total(),
            last_turn_usage: self.last_turn_usage().cloned(),
            context_tokens: self.context_tokens(),
        }
    }
}

/// What each part chooses to show the rest of the server.
///
/// Every one of these forwards to a method on the owning component's state;
/// none of them reaches a field. Adding a reader means adding a method there,
/// which is the point: the owner decides what is knowable about it.
impl AgentState {
    /// Accepted-but-undelivered things addressed to this agent, oldest first.
    #[must_use]
    pub fn inbox(&self) -> &[crate::agent_loop::Incoming] {
        self.part::<QueueState>().map_or(&[], QueueState::inbox)
    }

    /// Every question this agent is parked on, oldest first.
    #[must_use]
    pub fn asks(&self) -> &[crate::agent_loop::AskedQuestion] {
        self.part::<QueueState>().map_or(&[], QueueState::asks)
    }

    /// Whether the agent has parked itself awaiting something that will wake it.
    #[must_use]
    pub fn parked(&self) -> bool {
        self.part::<QueueState>().is_some_and(QueueState::parked)
    }

    /// True between a turn beginning and that turn reaching a boundary.
    #[must_use]
    pub fn turn_in_flight(&self) -> bool {
        self.part::<TurnState>().is_some_and(TurnState::in_flight)
    }

    /// Consecutive turns this agent ended without the result it owed.
    #[must_use]
    pub fn nudges(&self) -> u32 {
        self.part::<TurnState>().map_or(0, TurnState::nudges)
    }

    /// Active timers, durable so they re-arm on recovery.
    #[must_use]
    pub fn timers(&self) -> &[crate::agent_loop::components::timers::domain::TimerRecord] {
        self.part::<TimerState>().map_or(&[], TimerState::records)
    }

    /// The agent's own task list.
    #[must_use]
    pub fn task_list(&self) -> &crate::agent_loop::components::task_list::domain::TaskListState {
        match self.part::<TaskListPart>() {
            Some(part) => part.list(),
            None => crate::agent_loop::components::task_list::empty_list(),
        }
    }

    /// Cumulative token usage across every provider-backed history record.
    #[must_use]
    pub fn usage_total(&self) -> UsageTotal {
        self.usage_total
    }

    /// The newest completed provider step's usage.
    #[must_use]
    pub fn last_turn_usage(&self) -> Option<&Usage> {
        self.last_step_usage.as_ref()
    }

    #[must_use]
    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    pub(crate) fn bank_step_usage(&mut self, usage: Usage) {
        self.context_tokens = usage.input_tokens;
        self.usage_total.add(&usage);
        self.last_step_usage = Some(usage);
    }

    pub(crate) fn bank_usage(&mut self, usage: &Usage) {
        self.usage_total.add(usage);
    }

    pub(crate) fn context_is(&mut self, tokens: u32) {
        self.context_tokens = tokens;
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
    pub tasks: Vec<crate::agent_loop::components::task_list::domain::TaskRecord>,
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
mod params_tests {
    use crate::agent_loop::prelude::*;
    use crate::agent_loop::testing::*;
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod history_tests {
    use crate::agent_loop::prelude::*;
    use horsie_agentcore::{ContentPart, Message, Role};
    use horsie_models::agent::{TextPart, ToolCallPart, Usage};

    fn fold(state: AgentState, event: AgentDomainEvent) -> AgentState {
        AgentActor::apply_event(state, event)
    }

    #[test]
    fn seeding_adopts_history_without_recording_a_recursive_snapshot() {
        let source = fold(
            AgentActor::initial_state(),
            AgentDomainEvent::SystemPromptRecorded {
                source: SystemPromptSource::Configured,
                content: "fixed".into(),
            },
        );
        let seeded = fold(
            AgentActor::initial_state(),
            AgentDomainEvent::Seeded {
                state: Box::new(source.clone()),
                seed: None,
            },
        );
        assert_eq!(seeded.history().len(), source.history().len());
        assert!(matches!(
            &seeded.history()[0].record,
            AgentDomainEvent::SystemPromptRecorded { content, .. } if content == "fixed"
        ));
    }

    #[test]
    fn step_marker_sequence_is_its_identity() {
        let state = fold(
            AgentActor::initial_state(),
            AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            },
        );
        assert_eq!(state.open_step(), Some((0, &StepKind::Agent)));
        assert_eq!(state.next_history_seq(), 1);
    }

    #[test]
    fn initialization_records_immutable_prompt_bytes_once() {
        let events = [
            AgentDomainEvent::SystemPromptRecorded {
                source: SystemPromptSource::InitialContext,
                content: "fixed prompt".into(),
            },
            AgentDomainEvent::AgentInitialized {
                manifest: ContextManifest::default(),
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            },
            AgentDomainEvent::InputMessage {
                message: Message::user("u", "later", 1),
            },
        ];
        let state = events.into_iter().fold(AgentActor::initial_state(), fold);
        assert!(state.initialized());
        assert_eq!(state.system_prompt(), "fixed prompt");
    }

    #[test]
    fn every_provider_step_banks_usage_before_run_end() {
        let message = |id: &str| Message {
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Text(TextPart { text: "ok".into() })],
            created_at_ms: 1,
            started_at_ms: None,
        };
        let state = [
            AgentDomainEvent::MessageComplete {
                message: message("a1"),
                usage: Usage::without_cache(10, 2),
            },
            AgentDomainEvent::MessageComplete {
                message: message("a2"),
                usage: Usage::without_cache(20, 3),
            },
        ]
        .into_iter()
        .fold(AgentActor::initial_state(), fold);
        assert_eq!(state.usage_total().input_tokens, 30);
        assert_eq!(state.usage_total().output_tokens, 5);
        assert_eq!(state.context_tokens(), 20);
        assert!(
            !state
                .history()
                .iter()
                .any(|entry| matches!(&entry.record, AgentDomainEvent::RunEnded { .. }))
        );
    }

    #[test]
    fn explicit_workspace_inspection_is_ordinary_history() {
        let initial = fold(
            fold(
                AgentActor::initial_state(),
                AgentDomainEvent::SystemPromptRecorded {
                    source: SystemPromptSource::InitialContext,
                    content: "initial observation".into(),
                },
            ),
            AgentDomainEvent::AgentInitialized {
                manifest: ContextManifest::default(),
            },
        );
        let assistant = Message {
            id: "a".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "scan".into(),
                name: crate::agent_loop::INSPECT_WORKSPACE_TOOL.into(),
                input: serde_json::json!({}),
            })],
            created_at_ms: 2,
            started_at_ms: None,
        };
        let state = fold(
            fold(
                initial,
                AgentDomainEvent::MessageComplete {
                    message: assistant,
                    usage: Usage::without_cache(5, 1),
                },
            ),
            AgentDomainEvent::ToolComplete {
                tool_call_id: "scan".into(),
                output: "current workspace".into(),
                is_error: false,
                artifacts: Vec::new(),
                at_ms: 3,
            },
        );
        assert_eq!(state.system_prompt(), "initial observation");
        assert!(state.prompt_messages().iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::ToolResult(result) if result.tool_call_id == "scan"
                )
            })
        }));
    }

    #[test]
    fn compaction_and_seed_summary_bank_usage_independently() {
        let state = fold(
            AgentActor::initial_state(),
            AgentDomainEvent::Compacted {
                summary: "summary".into(),
                carried_state: String::new(),
                retained_from_message_id: None,
                trigger: horsie_agentcore::CompactionTrigger::Manual(
                    horsie_agentcore::EmptyOutcome {},
                ),
                instructions: None,
                tokens_before: 100,
                tokens_after: 20,
                usage: Some(Usage::without_cache(30, 4)),
                at_ms: 1,
            },
        );
        let state = fold(
            state,
            AgentDomainEvent::SeedSummaryTaken {
                request_id: "seed-1".into(),
                sub_sessions: Vec::new(),
                result: Ok("seed".into()),
                usage: Some(Usage::without_cache(7, 2)),
                at_ms: 2,
            },
        );
        assert_eq!(state.usage_total().input_tokens, 37);
        assert_eq!(state.usage_total().output_tokens, 6);
        assert_eq!(state.context_tokens(), 20);
    }
}
