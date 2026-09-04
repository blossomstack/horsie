//! Pure projection from durable agent history to the user-facing transcript.
//!
//! Transcript sequence numbers count only visible entries. History sequence
//! numbers count every domain record and remain the identity for foreground
//! steps. Keeping the projection here makes that distinction explicit and
//! prevents command handlers from maintaining a second durable log.

use crate::agent_loop::{AgentDomainEvent, AgentHistoryEntry, Incoming};
use horsie_agentcore::{
    AgentLogBody, AgentLogEntry, AskLifecycle, CompactionEntry, LifecycleEvent, Message,
    QueuedLifecycle, TurnBeganLifecycle,
};
use horsie_models::agent::TaskListLifecycle;

/// A deterministic user-facing projection of agent history.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    log: Vec<AgentLogEntry>,
    next_seq: u64,
}

impl Transcript {
    #[must_use]
    pub fn entries(&self) -> &[AgentLogEntry] {
        &self.log
    }

    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    #[must_use]
    pub fn tail_seq(&self) -> Option<u64> {
        self.log.last().map(|entry| entry.seq)
    }

    fn push(&mut self, at_ms: u64, body: AgentLogBody) {
        self.log.push(AgentLogEntry {
            seq: self.next_seq,
            at_ms,
            body,
        });
        self.next_seq = self.next_seq.saturating_add(1);
    }

    #[must_use]
    pub(crate) fn cut(&self, at_seq: u64) -> Self {
        Self {
            log: self
                .log
                .iter()
                .filter(|entry| entry.seq < at_seq)
                .cloned()
                .collect(),
            next_seq: at_seq,
        }
    }

    pub(crate) fn resolve_boundary(&self, retained_from_message_id: Option<&str>) -> (u64, u64) {
        let retain_nothing = (self.tail_seq().unwrap_or(0), self.next_seq());
        let Some(id) = retained_from_message_id else {
            return retain_nothing;
        };
        let Some(index) = self
            .log
            .iter()
            .position(|entry| entry.body.id().is_some_and(|found| found == id))
        else {
            tracing::warn!(
                message_id = id,
                "a compaction named a message this transcript does not hold; retaining nothing"
            );
            return retain_nothing;
        };
        let retained_from_seq = self.log[index].seq;
        let covers_through_seq = index
            .checked_sub(1)
            .map_or(retained_from_seq, |previous| self.log[previous].seq);
        (covers_through_seq, retained_from_seq)
    }
}

/// Build the complete visible transcript from durable history.
#[must_use]
pub fn project_transcript(history: &[AgentHistoryEntry]) -> Transcript {
    let mut transcript = Transcript::default();
    let mut pending_items = Vec::new();

    for entry in history {
        match &entry.record {
            AgentDomainEvent::Received { item, at_ms } => {
                pending_items.push(item.clone());
                if let Incoming::User { id, text, .. } = item {
                    transcript.push(
                        *at_ms,
                        AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(QueuedLifecycle {
                            id: id.clone(),
                            text: text.clone(),
                        })),
                    );
                }
            }
            AgentDomainEvent::Consumed { ids, .. } => {
                pending_items.retain(|item| !ids.iter().any(|id| id == item.id()));
            }
            AgentDomainEvent::TurnBegan {
                consumed,
                abandoned,
                rewritten,
                at_ms,
            } => {
                let selected: Vec<_> = pending_items
                    .iter()
                    .filter(|item| consumed.iter().any(|id| id == item.id()))
                    .cloned()
                    .collect();
                let visible = selected
                    .iter()
                    .filter_map(|item| match item {
                        Incoming::User { id, .. } => Some(id.clone()),
                        Incoming::SubAgent { .. }
                        | Incoming::Timer { .. }
                        | Incoming::Continue { .. }
                        | Incoming::Answers { .. }
                        | Incoming::Compact { .. }
                        | Incoming::SubSession { .. } => None,
                    })
                    .collect();
                let mut input = crate::agent_loop::run_loop::drain(&selected);
                input.abandoned.clone_from(abandoned);
                let answered = input
                    .answered
                    .iter()
                    .map(|answer| answer.tool_call_id.clone())
                    .collect();
                transcript.push(
                    *at_ms,
                    AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(TurnBeganLifecycle {
                        consumed: visible,
                        answered,
                    })),
                );
                for message in crate::agent_loop::run_loop::messages(
                    &input,
                    rewritten.as_deref(),
                    format!("turn:{}", entry.seq),
                    *at_ms,
                ) {
                    transcript.push(*at_ms, AgentLogBody::Llm(message));
                }
                pending_items.retain(|item| !consumed.iter().any(|id| id == item.id()));
            }
            AgentDomainEvent::AskRecorded { asks, at_ms } => {
                for ask in asks {
                    transcript.push(
                        *at_ms,
                        AgentLogBody::Lifecycle(LifecycleEvent::AskRecorded(AskLifecycle {
                            tool_call_id: ask.tool_call_id.clone(),
                            question: ask.question.clone(),
                        })),
                    );
                }
            }
            AgentDomainEvent::InputMessage { message }
            | AgentDomainEvent::MessageComplete { message, .. }
            | AgentDomainEvent::MessageAborted { message } => {
                transcript.push(message.created_at_ms, AgentLogBody::Llm(message.clone()));
            }
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
                artifacts,
                at_ms,
            } => transcript.push(
                *at_ms,
                AgentLogBody::Llm(Message::tool_result(
                    tool_call_id.clone(),
                    output.clone(),
                    *is_error,
                    artifacts.clone(),
                    *at_ms,
                )),
            ),
            AgentDomainEvent::HookRan { record, seq, at_ms } => transcript.push(
                *at_ms,
                AgentLogBody::Hook(hook_entry(record.clone(), *seq, *at_ms)),
            ),
            AgentDomainEvent::LifecycleRecorded { event, at_ms } => {
                transcript.push(*at_ms, AgentLogBody::Lifecycle(event.clone()));
            }
            AgentDomainEvent::TaskListChanged { snapshot, at_ms } => transcript.push(
                *at_ms,
                AgentLogBody::Lifecycle(LifecycleEvent::TaskList(TaskListLifecycle {
                    tasks: snapshot.wire_tasks(),
                })),
            ),
            AgentDomainEvent::Compacted {
                summary,
                carried_state,
                retained_from_message_id,
                trigger,
                instructions,
                tokens_before,
                tokens_after,
                at_ms,
                ..
            } => {
                let (covers_through_seq, retained_from_seq) =
                    transcript.resolve_boundary(retained_from_message_id.as_deref());
                transcript.push(
                    *at_ms,
                    AgentLogBody::Compaction(CompactionEntry {
                        summary: summary.clone(),
                        carried_state: carried_state.clone(),
                        covers_through_seq,
                        retained_from_seq,
                        trigger: trigger.clone(),
                        instructions: instructions.clone(),
                        tokens_before: *tokens_before,
                        tokens_after: *tokens_after,
                    }),
                );
            }
            AgentDomainEvent::SystemPromptRecorded { .. }
            | AgentDomainEvent::AgentInitialized { .. }
            | AgentDomainEvent::ConnectionCompleted
            | AgentDomainEvent::StepStarted { .. }
            | AgentDomainEvent::StepFailed { .. }
            | AgentDomainEvent::StopHookCompleted { .. }
            | AgentDomainEvent::RunEnded { .. }
            | AgentDomainEvent::Seeded { .. }
            | AgentDomainEvent::TurnCompleted { .. }
            | AgentDomainEvent::TurnAborted { .. }
            | AgentDomainEvent::TurnCancelled { .. }
            | AgentDomainEvent::TimerArmed { .. }
            | AgentDomainEvent::TimerCancelled { .. }
            | AgentDomainEvent::TimerFired { .. }
            | AgentDomainEvent::Parked { .. }
            | AgentDomainEvent::Nudged { .. }
            | AgentDomainEvent::SeedSummaryTaken { .. } => {}
        }
    }

    transcript
}

/// Convert a transcript prefix into conversation-history events for a branch.
/// Control records are deliberately absent, so no source run or pending input
/// becomes live in the new agent.
pub(crate) fn carry_transcript(transcript: &Transcript) -> Vec<AgentDomainEvent> {
    let mut hook_seq = 0;
    transcript
        .entries()
        .iter()
        .map(|entry| match &entry.body {
            AgentLogBody::Llm(message) => AgentDomainEvent::InputMessage {
                message: message.clone(),
            },
            AgentLogBody::Hook(hook) => {
                let event = AgentDomainEvent::HookRan {
                    record: hook.record.clone(),
                    seq: hook_seq,
                    at_ms: entry.at_ms,
                };
                hook_seq += 1;
                event
            }
            AgentLogBody::Lifecycle(event) => AgentDomainEvent::LifecycleRecorded {
                event: event.clone(),
                at_ms: entry.at_ms,
            },
            AgentLogBody::Compaction(compaction) => {
                let retained_from_message_id = transcript
                    .entries()
                    .iter()
                    .find(|candidate| candidate.seq == compaction.retained_from_seq)
                    .and_then(|candidate| candidate.body.id())
                    .map(str::to_string);
                AgentDomainEvent::Compacted {
                    summary: compaction.summary.clone(),
                    carried_state: compaction.carried_state.clone(),
                    retained_from_message_id,
                    trigger: compaction.trigger.clone(),
                    instructions: compaction.instructions.clone(),
                    tokens_before: compaction.tokens_before,
                    tokens_after: compaction.tokens_after,
                    usage: None,
                    at_ms: entry.at_ms,
                }
            }
        })
        .collect()
}

#[must_use]
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

#[must_use]
pub fn hook_entry_id(seq: usize) -> String {
    format!("hook:{seq}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use crate::agent_loop::run_loop::RunLoop;
    use crate::agent_loop::{AgentState, RunEnd, StepKind};
    use horsie_agentcore::{EmptyOutcome, Role, TurnOutcome};

    fn fold(events: impl IntoIterator<Item = AgentDomainEvent>) -> AgentState {
        events
            .into_iter()
            .fold(AgentState::default(), RunLoop::apply)
    }

    #[test]
    fn projection_keeps_visible_order_and_uses_dense_cursors() {
        let state = fold([
            AgentDomainEvent::StepStarted {
                kind: StepKind::Provider,
            },
            AgentDomainEvent::Received {
                item: Incoming::User {
                    id: "incoming-1".into(),
                    text: "hello".into(),
                    artifacts: Vec::new(),
                },
                at_ms: 1,
            },
            AgentDomainEvent::TurnBegan {
                consumed: vec!["incoming-1".into()],
                abandoned: Vec::new(),
                rewritten: None,
                at_ms: 2,
            },
            AgentDomainEvent::MessageComplete {
                message: Message::assistant_text("assistant-1", "hi", 4),
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            },
            AgentDomainEvent::TurnCompleted {
                iterations: 1,
                at_ms: 5,
            },
            AgentDomainEvent::RunEnded {
                reason: RunEnd::Complete {
                    output: serde_json::Value::Null,
                },
                at_ms: 6,
            },
        ]);

        let transcript = project_transcript(state.history());
        assert_eq!(
            transcript
                .entries()
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(matches!(
            &transcript.entries()[0].body,
            AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(_))
        ));
        assert!(matches!(
            &transcript.entries()[1].body,
            AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(_))
        ));
        assert!(matches!(
            &transcript.entries()[2].body,
            AgentLogBody::Llm(message) if message.role == Role::User
        ));
        assert!(matches!(
            &transcript.entries()[3].body,
            AgentLogBody::Llm(message) if message.role == Role::Assistant
        ));
    }

    #[test]
    fn agent_state_serializes_history_without_a_transcript_copy() {
        let state = fold([
            AgentDomainEvent::Received {
                item: Incoming::User {
                    id: "incoming-1".into(),
                    text: "unique payload".into(),
                    artifacts: Vec::new(),
                },
                at_ms: 1,
            },
            AgentDomainEvent::TurnBegan {
                consumed: vec!["incoming-1".into()],
                abandoned: Vec::new(),
                rewritten: None,
                at_ms: 2,
            },
        ]);
        let expected = state.transcript();
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("transcript"), "{json}");
        assert_eq!(json.matches("unique payload").count(), 1, "{json}");
        let restored: AgentState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.transcript(), expected);
    }

    #[test]
    fn input_after_a_marker_first_reaches_the_next_provider_step() {
        let state = fold([
            AgentDomainEvent::Received {
                item: Incoming::User {
                    id: "first".into(),
                    text: "first input".into(),
                    artifacts: Vec::new(),
                },
                at_ms: 1,
            },
            AgentDomainEvent::TurnBegan {
                consumed: vec!["first".into()],
                abandoned: Vec::new(),
                rewritten: None,
                at_ms: 2,
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Provider,
            },
            AgentDomainEvent::Received {
                item: Incoming::User {
                    id: "later".into(),
                    text: "later input".into(),
                    artifacts: Vec::new(),
                },
                at_ms: 3,
            },
            AgentDomainEvent::MessageComplete {
                message: Message::assistant_text("assistant", "done", 4),
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            },
            AgentDomainEvent::TurnCompleted {
                iterations: 1,
                at_ms: 4,
            },
            AgentDomainEvent::TurnBegan {
                consumed: vec!["later".into()],
                abandoned: Vec::new(),
                rewritten: None,
                at_ms: 5,
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Provider,
            },
        ]);

        let text_through = |marker| {
            state
                .prompt_messages_through(marker)
                .into_iter()
                .flat_map(|message| message.parts)
                .filter_map(|part| match part {
                    horsie_agentcore::ContentPart::Text(text) => Some(text.text),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(text_through(2), ["first input"]);
        assert_eq!(text_through(7), ["first input", "done", "later input"]);
    }

    #[test]
    fn a_branch_carries_visible_context_without_source_control_state() {
        let source = fold([
            AgentDomainEvent::Received {
                item: Incoming::User {
                    id: "incoming-1".into(),
                    text: "hello".into(),
                    artifacts: Vec::new(),
                },
                at_ms: 1,
            },
            AgentDomainEvent::TurnBegan {
                consumed: vec!["incoming-1".into()],
                abandoned: Vec::new(),
                rewritten: None,
                at_ms: 2,
            },
            AgentDomainEvent::Compacted {
                summary: "summary".into(),
                carried_state: String::new(),
                retained_from_message_id: Some("turn:1".into()),
                trigger: horsie_agentcore::CompactionTrigger::Manual(EmptyOutcome {}),
                instructions: None,
                tokens_before: 10,
                tokens_after: 3,
                usage: None,
                at_ms: 4,
            },
            AgentDomainEvent::LifecycleRecorded {
                event: LifecycleEvent::TurnEnded(horsie_agentcore::TurnEndedLifecycle {
                    outcome: TurnOutcome::Ended(EmptyOutcome {}),
                }),
                at_ms: 4,
            },
        ]);
        let expected = source.transcript();
        let branch = source.snapshot_at(expected.next_seq());

        assert_eq!(branch.transcript(), expected);
        assert!(branch.pending_incoming().is_empty());
        assert!(branch.open_step().is_none());
        assert!(!branch.initialized());
    }

    #[test]
    fn a_seed_message_becomes_history_before_it_becomes_transcript() {
        let seeded = fold([
            AgentDomainEvent::Seeded {
                state: Box::new(AgentState::default()),
            },
            AgentDomainEvent::InputMessage {
                message: Message::user("seed", "summary", 1),
            },
        ]);

        assert!(matches!(
            seeded.history(),
            [AgentHistoryEntry {
                record: AgentDomainEvent::InputMessage { message },
                ..
            }] if message.id == "seed"
        ));
        assert_eq!(seeded.transcript().entries().len(), 1);
    }

    #[test]
    fn projection_is_deterministic_across_repeated_reads() {
        let state = fold([
            AgentDomainEvent::InputMessage {
                message: Message::user("u", "hello", 1),
            },
            AgentDomainEvent::Compacted {
                summary: "summary".into(),
                carried_state: String::new(),
                retained_from_message_id: Some("u".into()),
                trigger: horsie_agentcore::CompactionTrigger::Manual(EmptyOutcome {}),
                instructions: None,
                tokens_before: 10,
                tokens_after: 3,
                usage: None,
                at_ms: 2,
            },
        ]);
        assert_eq!(state.transcript(), state.transcript());
    }
}
