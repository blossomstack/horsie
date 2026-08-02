//! Event plumbing for session SSE streams.
//!
//! Durable coarse events are read from the agent journal with stable sequence
//! ids (the SSE cursor space — interactive agents never compact, so ids are
//! exact journal positions forever). Ephemeral deltas ride the live broadcast
//! without ids.

use crate::sessions::SessionFrame;
use async_trait::async_trait;
use horsie_agentcore::{AgentEvent, EventSink, EventSinkError};
use horsie_models::session::{
    MessageEvent, SessionEvent, TaskItem, TaskListEvent, TaskStatus as WireTaskStatus,
    ToolOutputEvent, TurnCompletedEvent,
};
use horsie_workflow::{AgentDomainEvent, TaskStatus as AgentTaskStatus};
use tokio::sync::broadcast;

/// Forwards live agent events into the session's broadcast: deltas pass through
/// id-less; journaled coarse events become `Journaled` wakeups (SSE handlers
/// re-read the journal for stable ids). Ordering note: the agent's `PersistSink`
/// persists each coarse event *before* forwarding here, so a `Journaled` wakeup
/// always finds the event already durable. Best-effort — never aborts the run.
pub struct SessionEventSink {
    pub frames: broadcast::Sender<SessionFrame>,
}

#[async_trait]
impl EventSink for SessionEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        let frame = match &event {
            AgentEvent::TextChunk(e) => Some(SessionFrame::Delta {
                text: e.text.clone(),
            }),
            AgentEvent::ToolCallStart(e) => Some(SessionFrame::ToolStart {
                tool_call_id: e.tool_call_id.clone(),
                name: e.name.clone(),
            }),
            AgentEvent::InputMessage(_)
            | AgentEvent::MessageComplete(_)
            | AgentEvent::ToolComplete(_)
            | AgentEvent::RunComplete(_) => Some(SessionFrame::Journaled),
            AgentEvent::MessageStart(_)
            | AgentEvent::MessageStop(_)
            | AgentEvent::TextBlockStart(_)
            | AgentEvent::ThinkingBlockStart(_)
            | AgentEvent::ThinkingChunk(_)
            | AgentEvent::ThinkingSignatureChunk(_)
            | AgentEvent::ToolCallInputDelta(_)
            | AgentEvent::ContentBlockStop(_)
            | AgentEvent::ToolExecuting(_) => None,
        };
        if let Some(f) = frame {
            let _ = self.frames.send(f);
        }
        Ok(())
    }
}

/// A subagent's observation sink: quiet by design. A subagent's streaming
/// events never reach the session broadcast — only the spawn/finish
/// progression frames the session itself emits surface there.
pub struct QuietEventSink;

#[async_trait]
impl EventSink for QuietEventSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

/// A coarse event replayed from the agent journal, with its stable sequence id.
#[derive(Debug, Clone)]
pub struct StampedEvent {
    pub seq: u64,
    pub event: SessionEvent,
}

/// Map one journaled agent event onto its wire shape (`None` = not surfaced).
pub(crate) fn wire_event(event: AgentDomainEvent) -> Option<SessionEvent> {
    match event {
        AgentDomainEvent::InputMessage { mut message }
        | AgentDomainEvent::MessageComplete { mut message } => {
            crate::wire_redact::strip_message_signature(&mut message);
            Some(SessionEvent::Message(MessageEvent { message }))
        }
        AgentDomainEvent::ToolComplete {
            tool_call_id,
            output,
            is_error,
        } => Some(SessionEvent::ToolResult(ToolOutputEvent {
            tool_call_id,
            output,
            is_error,
        })),
        AgentDomainEvent::RunComplete {
            usage, iterations, ..
        } => Some(SessionEvent::TurnCompleted(TurnCompletedEvent {
            iterations,
            usage,
        })),
        AgentDomainEvent::TaskListChanged { snapshot } => {
            Some(SessionEvent::TaskListChanged(TaskListEvent {
                tasks: snapshot
                    .tasks()
                    .iter()
                    .map(|t| TaskItem {
                        id: t.id,
                        content: t.content.clone(),
                        status: wire_task_status(t.status),
                    })
                    .collect(),
            }))
        }
        AgentDomainEvent::RunCancelled
        | AgentDomainEvent::TimerArmed { .. }
        | AgentDomainEvent::TimerCancelled { .. }
        | AgentDomainEvent::TimerFired { .. }
        | AgentDomainEvent::Parked => None,
    }
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

    #[test]
    fn wire_event_maps_journaled_events_and_drops_the_rest() {
        // Sequence numbering lives with the agent that owns the journal (see
        // `an_agent_replays_its_own_journal_after_a_cursor`); what is left here
        // is the mapping onto the wire, including the events that produce no
        // frame at all.
        let msg = horsie_models::agent::Message::user("m1", "hello");
        match wire_event(AgentDomainEvent::InputMessage {
            message: msg.clone(),
        }) {
            Some(SessionEvent::Message(m)) => assert_eq!(m.message.id, "m1"),
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(
            wire_event(AgentDomainEvent::RunCancelled).is_none(),
            "a cancellation has no wire shape, but still consumed a sequence number"
        );
        match wire_event(AgentDomainEvent::ToolComplete {
            tool_call_id: "tc".into(),
            output: "ok".into(),
            is_error: false,
        }) {
            Some(SessionEvent::ToolResult(e)) => assert_eq!(e.tool_call_id, "tc"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn task_list_changed_maps_to_wire_event() {
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

        match wire_event(AgentDomainEvent::TaskListChanged { snapshot }) {
            Some(SessionEvent::TaskListChanged(e)) => {
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
    fn wire_event_strips_thinking_signature() {
        use horsie_models::agent::{ContentPart, Message, Role, ThinkingPart};

        let message = Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Thinking(ThinkingPart {
                text: "reasoning".into(),
                signature: Some("opaque-blob".into()),
            })],
        };
        let wired = wire_event(AgentDomainEvent::MessageComplete { message })
            .expect("MessageComplete should surface");
        match wired {
            SessionEvent::Message(m) => match &m.message.parts[0] {
                ContentPart::Thinking(th) => {
                    assert_eq!(th.signature, None);
                    assert_eq!(th.text, "reasoning");
                }
                other => panic!("expected Thinking, got {other:?}"),
            },
            other => panic!("expected Message, got {other:?}"),
        }
    }
}
