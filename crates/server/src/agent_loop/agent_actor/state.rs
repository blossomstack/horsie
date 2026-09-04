//! The transcript and the running totals: what folding an agent's events
//! leaves behind.
//!
//! [`AgentState`] is a durability contract — it is snapshotted, so a field that
//! fails to deserialize takes down `recover()` for every existing session. Add
//! optional fields; never rename or repurpose one.

use super::*;
use horsie_agentcore::{AgentLogBody, AgentLogEntry, Usage};
use serde::{Deserialize, Serialize};

/// The session history reconstructed by folding [`AgentDomainEvent`]s, plus
/// any timers the agent has armed and whether it is currently parked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// The transcript: everything the user sees, whether or not the model saw
    /// it. Read [`Self::prompt_messages`] to get what goes to a provider — this
    /// field deliberately cannot be handed to one.
    ///
    /// Every field here carries `#[serde(default)]`, including this one: state
    /// is snapshotted, so it is a durability contract. A field that fails to
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
    /// addressed to an *agent*: once one can name a subagent or a workflow
    /// step, a session-level queue has nowhere to put it. Durable for the same
    /// reason timers are — an accepted message is a promise, and a crash must
    /// not forget it.
    #[serde(default)]
    pub inbox: Vec<crate::agent_loop::Incoming>,
    /// Every question this agent is parked on, oldest first. A turn may ask
    /// several at once, and the run cannot resume until all of them have a
    /// result.
    #[serde(default)]
    pub asks: Vec<crate::agent_loop::AskedQuestion>,
    /// Active timers — durable so they re-arm on recovery and back
    /// `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::agent_loop::timers::TimerRecord>,
    /// True while the agent has parked itself awaiting a timer (no run in
    /// flight).
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
    /// Cumulative token usage across every completed run — durable agent
    /// state, folded from `RunComplete`. `u64` so a long session's
    /// re-sent-context input total can't overflow the per-turn `u32` wire
    /// counters. Answers the session's usage readout without replaying the
    /// whole journal.
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
    /// Durable operational counters used to explain costly runs.
    #[serde(default)]
    pub efficiency: AgentEfficiencyStats,
}

/// Cumulative counters that explain an agent's execution cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEfficiencyStats {
    pub provider_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub tool_result_bytes: u64,
    pub completed_runs: u64,
    pub aborted_runs: u64,
    pub compactions: u64,
}

impl AgentEfficiencyStats {
    pub(super) fn observe(&mut self, event: &AgentDomainEvent) {
        match event {
            AgentDomainEvent::MessageComplete { message }
            | AgentDomainEvent::MessageAborted { message } => {
                self.provider_calls = self.provider_calls.saturating_add(1);
                let calls = message
                    .parts
                    .iter()
                    .filter(|part| matches!(part, horsie_agentcore::ContentPart::ToolCall(_)))
                    .count() as u64;
                self.tool_calls = self.tool_calls.saturating_add(calls);
            }
            AgentDomainEvent::ToolComplete {
                output, is_error, ..
            } => {
                self.tool_result_bytes = self.tool_result_bytes.saturating_add(output.len() as u64);
                if *is_error {
                    self.failed_tool_calls = self.failed_tool_calls.saturating_add(1);
                }
            }
            AgentDomainEvent::RunComplete { .. } => {
                self.completed_runs = self.completed_runs.saturating_add(1);
            }
            AgentDomainEvent::RunAborted { .. } | AgentDomainEvent::RunCancelled { .. } => {
                self.aborted_runs = self.aborted_runs.saturating_add(1);
            }
            AgentDomainEvent::Compacted { .. } => {
                self.compactions = self.compactions.saturating_add(1);
            }
            AgentDomainEvent::Seeded { .. }
            | AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::TimerArmed { .. }
            | AgentDomainEvent::TimerCancelled { .. }
            | AgentDomainEvent::TimerFired { .. }
            | AgentDomainEvent::TaskListChanged { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
            | AgentDomainEvent::Received { .. }
            | AgentDomainEvent::TurnBegan { .. }
            | AgentDomainEvent::AskRecorded { .. }
            | AgentDomainEvent::Parked { .. }
            | AgentDomainEvent::Nudged { .. } => {}
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
            efficiency: self.efficiency,
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
            efficiency: self.efficiency,
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
    use super::*;
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

        let tail = crate::agent_loop::agent_log::page(
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
        let forward = crate::agent_loop::agent_log::since(&state.log, 1);
        assert_eq!(forward.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

        let back = crate::agent_loop::agent_log::page(
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
    fn efficiency_counters_are_rebuilt_from_durable_events() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::MessageComplete {
                message: Message {
                    id: "assistant".into(),
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolCall(ToolCallPart {
                        id: "call".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    })],
                    created_at_ms: 1,
                    started_at_ms: None,
                },
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::ToolComplete {
                tool_call_id: "call".into(),
                output: "failed".into(),
                is_error: true,
                artifacts: Vec::new(),
                at_ms: 2,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(10, 2),
                iterations: 1,
                context_tokens: 10,
                at_ms: 3,
            },
        );

        assert_eq!(state.efficiency.provider_calls, 1);
        assert_eq!(state.efficiency.tool_calls, 1);
        assert_eq!(state.efficiency.failed_tool_calls, 1);
        assert_eq!(state.efficiency.tool_result_bytes, 6);
        assert_eq!(state.efficiency.completed_runs, 1);
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
