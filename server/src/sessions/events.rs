//! Task-list wire mapping, and the session-state fold used by tests.
//!
//! The broadcast plumbing that used to live here — an ephemeral event sink, a
//! post-persist observer, and the frame unions both fed — is gone. Durable
//! entries are read from the agent's own state through `AgentCommand::ReadLog`,
//! and deltas reach the agent through its mailbox, so there is no second copy
//! of the transcript in flight to keep ordered against the first.

use horsie_models::agent::{TaskItem, TaskStatus as WireTaskStatus};
use horsie_workflow::TaskStatus as AgentTaskStatus;

pub(crate) fn wire_task(t: &horsie_workflow::TaskRecord) -> TaskItem {
    TaskItem {
        id: t.id,
        content: t.content.clone(),
        status: wire_task_status(t.status),
    }
}

/// Fold a session's own journal into its [`SessionState`] — **tests only**.
///
/// Production reads a session's state by asking its actor, which is the only
/// thing allowed to read that journal. A test asserting on what was journaled
/// has no actor to ask (and often deliberately none running), so it folds
/// directly; `cfg(test)` is what keeps that from becoming a production path
/// again. See docs/superpowers/specs/2026-08-02-answerable-asks-design.md.
#[cfg(test)]
pub(in crate::sessions) async fn fold_session_state(
    journal: &std::sync::Arc<dyn horsie_actor::Journal>,
    session_id: uuid::Uuid,
) -> crate::sessions::session_actor::SessionState {
    use futures_util::StreamExt;
    use horsie_actor::EventSourcedActor;

    let pid = crate::sessions::session_actor::SessionActor::persistence_id_for(session_id);
    let mut state = crate::sessions::session_actor::SessionState::default();
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection of a journal, with no actor running to ask"
    )]
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        let Ok((_seq, bytes)) = item else { break };
        if let Ok(event) =
            serde_json::from_slice::<crate::sessions::session_actor::SessionDomainEvent>(&bytes)
        {
            state = crate::sessions::session_actor::SessionActor::apply_event(state, event);
        }
    }
    state
}

fn wire_task_status(status: AgentTaskStatus) -> WireTaskStatus {
    match status {
        AgentTaskStatus::Pending => WireTaskStatus::Pending,
        AgentTaskStatus::InProgress => WireTaskStatus::InProgress,
        AgentTaskStatus::Completed => WireTaskStatus::Completed,
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

    /// Every transcript-bearing event becomes exactly one `Appended`, matching
    /// `AgentActor::apply_event`, which pushes exactly one message for each.
    #[test]
    fn transcript_events_all_become_one_append() {
        let msg = horsie_models::agent::Message::user("m1", "hello", 0);
        match agent_frame(&AgentDomainEvent::InputMessage {
            message: msg.clone(),
        }) {
            Some(AgentFrame::Appended { entry }) => assert_eq!(entry.id(), "m1"),
            other => panic!("expected Appended, got {other:?}"),
        }
        // A tool result is an append too — reconstructed exactly as the fold
        // does, so the stream and `/history` agree on its id.
        match agent_frame(&AgentDomainEvent::ToolComplete {
            at_ms: 7,
            tool_call_id: "tc".into(),
            output: "ok".into(),
            is_error: false,
        }) {
            Some(AgentFrame::Appended {
                entry: HistoryEntry::Llm(message),
            }) => {
                assert_eq!(message.id, "result:tc");
                assert_eq!(message.created_at_ms, 7);
            }
            other => panic!("expected Appended, got {other:?}"),
        }
        assert!(
            agent_frame(&AgentDomainEvent::RunCancelled { at_ms: 0 }).is_none(),
            "a cancellation has no wire shape"
        );
    }

    #[test]
    fn the_wire_carries_the_stamp_off_the_journaled_event() {
        // The SSE path never reads the clock: an event must report when it
        // happened, not when it was broadcast.
        match agent_frame(&AgentDomainEvent::RunComplete {
            at_ms: 1_700_000_009_999,
            usage: horsie_models::agent::Usage::without_cache(1, 1),
            iterations: 2,
            context_tokens: 3,
        }) {
            Some(AgentFrame::TurnCompleted { at_ms, .. }) => assert_eq!(at_ms, 1_700_000_009_999),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    /// A hook record is an append, so a client watching live ends up with the
    /// same transcript a reload would fetch. The id must match what the fold
    /// derives, or the stream and `/history` disagree on the cursor.
    #[test]
    fn a_hook_record_is_an_append_with_the_id_the_fold_derives() {
        let record = horsie_models::hooks::HookRecord {
            plugin: "guard".into(),
            duration_ms: 4,
            halt: None,
            action: horsie_models::hooks::HookAction::PreToolUse(
                horsie_models::hooks::PreToolUseRecord {
                    call: horsie_models::hooks::ToolScope {
                        tool: "bash".into(),
                        tool_call_id: "tc1".into(),
                    },
                    system_message: None,
                    outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                        horsie_models::hooks::HookDenied {
                            reason: Some("not allowed".into()),
                        },
                    ),
                },
            ),
        };
        match agent_frame(&AgentDomainEvent::HookRan {
            record: record.clone(),
            seq: 1,
            at_ms: 42,
        }) {
            Some(AgentFrame::Appended {
                entry: HistoryEntry::Hook(hook),
            }) => {
                assert_eq!(hook.id, "hook:1");
                assert_eq!(hook.created_at_ms, 42);
                assert_eq!(hook.record.plugin, "guard");
                match &hook.record.action {
                    horsie_models::hooks::HookAction::PreToolUse(r) => {
                        assert!(matches!(
                            r.outcome,
                            horsie_models::hooks::PreToolUseOutcome::Denied(_)
                        ));
                    }
                    other => panic!("expected a PreToolUse action, got {other:?}"),
                }
            }
            other => panic!("expected a hook append, got {other:?}"),
        }
    }

    #[test]
    fn a_transcript_message_carries_its_own_stamps() {
        // `/history` is answered from agent state, so the stamps must live on
        // the message itself rather than beside it on the event.
        let mut message = horsie_models::agent::Message::user("m1", "hello", 1_700_000_000_001);
        message.started_at_ms = Some(1_700_000_000_000);
        match agent_frame(&AgentDomainEvent::MessageComplete { message }) {
            Some(AgentFrame::Appended {
                entry: HistoryEntry::Llm(message),
            }) => {
                assert_eq!(message.created_at_ms, 1_700_000_000_001);
                assert_eq!(message.started_at_ms, Some(1_700_000_000_000));
            }
            other => panic!("expected Appended, got {other:?}"),
        }
    }

    #[test]
    fn task_list_changed_maps_to_the_whole_list() {
        let mut snapshot = horsie_workflow::TaskListState::default();
        snapshot
            .apply(horsie_workflow::TaskListAction::Create {
                tasks: vec!["a".into(), "b".into()],
            })
            .unwrap();
        snapshot
            .apply(horsie_workflow::TaskListAction::UpdateStatus {
                ids: vec![1],
                status: horsie_workflow::TaskStatus::Completed,
            })
            .unwrap();

        let frame = agent_frame(&AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot })
            .expect("a task-list change surfaces");
        match wire_agent_frame(frame) {
            AgentStreamEvent::TaskListChanged(e) => {
                assert_eq!(e.tasks.len(), 2);
                assert_eq!(e.tasks[0].id, 1);
                assert_eq!(e.tasks[0].content, "a");
                assert_eq!(e.tasks[0].status, WireTaskStatus::Completed);
                assert_eq!(e.tasks[1].status, WireTaskStatus::Pending);
            }
            other => panic!("expected TaskListChanged, got {other:?}"),
        }
    }

    #[test]
    fn an_append_strips_the_thinking_signature() {
        use horsie_models::agent::{ContentPart, Message, Role, ThinkingPart};

        let message = Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Thinking(ThinkingPart {
                text: "reasoning".into(),
                signature: Some("opaque-blob".into()),
            })],
        };
        let frame = agent_frame(&AgentDomainEvent::MessageComplete { message })
            .expect("MessageComplete should surface");
        match frame {
            AgentFrame::Appended {
                entry: HistoryEntry::Llm(message),
            } => match &message.parts[0] {
                ContentPart::Thinking(th) => {
                    assert_eq!(th.signature, None);
                    assert_eq!(th.text, "reasoning");
                }
                other => panic!("expected Thinking, got {other:?}"),
            },
            other => panic!("expected Appended, got {other:?}"),
        }
    }
}
