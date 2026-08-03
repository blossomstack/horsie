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
            at_ms,
        } => Some(SessionEvent::ToolResult(ToolOutputEvent {
            tool_call_id,
            output,
            is_error,
            at_ms,
        })),
        AgentDomainEvent::RunComplete {
            usage,
            iterations,
            at_ms,
            ..
        } => Some(SessionEvent::TurnCompleted(TurnCompletedEvent {
            iterations,
            usage,
            at_ms,
        })),
        AgentDomainEvent::TaskListChanged { snapshot, .. } => {
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
        AgentDomainEvent::RunCancelled { .. }
        | AgentDomainEvent::TimerArmed { .. }
        | AgentDomainEvent::TimerCancelled { .. }
        | AgentDomainEvent::TimerFired { .. }
        | AgentDomainEvent::Parked { .. } => None,
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
        let Ok(bytes) = item else { break };
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

    #[test]
    fn wire_event_maps_journaled_events_and_drops_the_rest() {
        // Sequence numbering lives with the agent that owns the journal (see
        // `an_agent_replays_its_own_journal_after_a_cursor`); what is left here
        // is the mapping onto the wire, including the events that produce no
        // frame at all.
        let msg = horsie_models::agent::Message::user("m1", "hello", 0);
        match wire_event(AgentDomainEvent::InputMessage {
            message: msg.clone(),
        }) {
            Some(SessionEvent::Message(m)) => assert_eq!(m.message.id, "m1"),
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(
            wire_event(AgentDomainEvent::RunCancelled { at_ms: 0 }).is_none(),
            "a cancellation has no wire shape, but still consumed a sequence number"
        );
        match wire_event(AgentDomainEvent::ToolComplete {
            at_ms: 0,
            tool_call_id: "tc".into(),
            output: "ok".into(),
            is_error: false,
        }) {
            Some(SessionEvent::ToolResult(e)) => assert_eq!(e.tool_call_id, "tc"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn the_wire_carries_the_stamp_off_the_journaled_event() {
        // The SSE path never reads the clock: a replayed event must report
        // when it happened, not when the client reconnected.
        match wire_event(AgentDomainEvent::ToolComplete {
            at_ms: 1_700_000_000_123,
            tool_call_id: "tc".into(),
            output: "ok".into(),
            is_error: false,
        }) {
            Some(SessionEvent::ToolResult(e)) => assert_eq!(e.at_ms, 1_700_000_000_123),
            other => panic!("expected ToolResult, got {other:?}"),
        }
        match wire_event(AgentDomainEvent::RunComplete {
            at_ms: 1_700_000_009_999,
            usage: horsie_models::agent::Usage::without_cache(1, 1),
            iterations: 2,
            context_tokens: 3,
        }) {
            Some(SessionEvent::TurnCompleted(e)) => assert_eq!(e.at_ms, 1_700_000_009_999),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn a_transcript_message_carries_its_own_stamps() {
        // `/history` is answered from agent state, so the stamps must live on
        // the message itself rather than beside it on the event.
        let mut message = horsie_models::agent::Message::user("m1", "hello", 1_700_000_000_001);
        message.started_at_ms = Some(1_700_000_000_000);
        match wire_event(AgentDomainEvent::MessageComplete { message }) {
            Some(SessionEvent::Message(m)) => {
                assert_eq!(m.message.created_at_ms, 1_700_000_000_001);
                assert_eq!(m.message.started_at_ms, Some(1_700_000_000_000));
            }
            other => panic!("expected Message, got {other:?}"),
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

        match wire_event(AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot }) {
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
            created_at_ms: 0,
            started_at_ms: None,
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
