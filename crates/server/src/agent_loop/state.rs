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
            log: self.log.iter().filter(|e| e.seq < at_seq).cloned().collect(),
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
            transcript: self.transcript.cut(at_seq),
            parts: self.parts.iter().filter_map(ComponentState::carried).collect(),
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

    /// Cumulative token usage across every completed turn.
    #[must_use]
    pub fn usage_total(&self) -> UsageTotal {
        self.part::<UsageState>()
            .map_or_else(UsageTotal::default, UsageState::total)
    }

    /// The most recently completed turn's own usage, summed across its calls
    /// but never across turns.
    #[must_use]
    pub fn last_turn_usage(&self) -> Option<&Usage> {
        self.part::<UsageState>().and_then(UsageState::last_turn)
    }

    /// The last provider call's prompt size alone — what is loaded in this
    /// agent's context right now.
    #[must_use]
    pub fn context_tokens(&self) -> u32 {
        self.part::<UsageState>().map_or(0, UsageState::context_tokens)
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
mod tests {
    use crate::agent_loop::prelude::*;
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use crate::agent_loop::prelude::*;
    use crate::agent_loop::agent_actor::testing::*;
    use horsie_agentcore::{ContentPart, LifecycleEvent, Message, Role};
    use horsie_models::agent::{
        ArtifactKind, ArtifactRef, ImageArtifact, ToolCallPart, ToolResultPart, Usage,
    };

    #[test]
    fn a_replayed_tool_result_keeps_its_original_stamp_and_artifacts() {
        let artifact = ArtifactRef {
            id: "image-id".into(),
            media_type: "image/png".into(),
            kind: ArtifactKind::Image(ImageArtifact {
                width: Some(640),
                height: Some(480),
            }),
            byte_size: 12,
            filename: Some("page.png".into()),
        };
        let fold = || {
            AgentActor::apply_event(
                AgentActor::initial_state(),
                AgentDomainEvent::ToolComplete {
                    at_ms: 1_700_000_000_123,
                    tool_call_id: "tc1".into(),
                    output: "Image loaded.".into(),
                    is_error: false,
                    artifacts: vec![artifact.clone()],
                },
            )
        };
        let first = fold();
        let second = fold();
        assert_eq!(first.log[0].at_ms, 1_700_000_000_123);
        assert_eq!(first.log, second.log);
        let AgentLogBody::Llm(message) = &first.log[0].body else {
            panic!("expected tool result message")
        };
        let ContentPart::ToolResult(result) = &message.parts[0] else {
            panic!("expected tool result part")
        };
        assert_eq!(result.artifacts, vec![artifact]);
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

    /// The transcript is not the session: a translated entry keeps its
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
    /// about two disjoint id spaces (`result:{tool_call_id}` and `hook:{n}`)
    /// to stay unambiguous; one counter over all of them has nothing to
    /// disambiguate.
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
                artifacts: Vec::new(),
                at_ms: 9,
            },
        );

        assert_eq!(
            state.log.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "message, hook and tool result each take one number"
        );
        assert_eq!(state.next_seq, 3);

        let tail = crate::agent_loop::shared::agent_log::page(
            &state.log,
            crate::agent_loop::Anchor::Tail,
            2,
            &crate::agent_loop::LogFilter::everything(),
        );
        assert_eq!(
            tail.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // The cursor resolves against a hook entry exactly like a message.
        let forward = crate::agent_loop::shared::agent_log::since(&state.log, 1);
        assert_eq!(forward.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

        let back = crate::agent_loop::shared::agent_log::page(
            &state.log,
            crate::agent_loop::Anchor::Before(1),
            10,
            &crate::agent_loop::LogFilter::everything(),
        );
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
                    artifacts: Vec::new(),
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
                artifacts: Vec::new(),
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
}
