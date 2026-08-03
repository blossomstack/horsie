//! Event plumbing for session and agent SSE streams.
//!
//! Two seams, deliberately separate. The agent's *streaming* events (text
//! deltas, tool starts) ride [`AgentEventSink`] and are ephemeral. Its *durable*
//! events arrive through [`BroadcastObserver`], which the agent actor calls from
//! its post-persist hook — so a broadcast append is already journaled and
//! already folded, and nothing has to re-read the journal to learn about it.

use crate::sessions::AgentFrame;
use async_trait::async_trait;
use horsie_agentcore::{AgentEvent, EventSink, EventSinkError, Message};
use horsie_models::session::{
    AgentStreamEvent, AppendedEvent, DeltaEvent, ResyncEvent, TaskItem, TaskListEvent,
    TaskStatus as WireTaskStatus, ToolStartEvent, TurnCompletedEvent,
};
use horsie_workflow::{AgentDomainEvent, AgentObserver, AgentState, TaskStatus as AgentTaskStatus};
use tokio::sync::broadcast;

/// Forwards an agent's *ephemeral* streaming events to its broadcast. Coarse
/// events are deliberately dropped here — they reach the stream through
/// [`BroadcastObserver`] once durable, so publishing them here too would
/// double-send and, worse, send them before they were written.
pub struct AgentEventSink {
    pub frames: broadcast::Sender<AgentFrame>,
}

#[async_trait]
impl EventSink for AgentEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        let frame = match &event {
            AgentEvent::TextChunk(e) => Some(AgentFrame::Delta {
                text: e.text.clone(),
            }),
            AgentEvent::ToolCallStart(e) => Some(AgentFrame::ToolStart {
                tool_call_id: e.tool_call_id.clone(),
                name: e.name.clone(),
            }),
            // Durable events: published by the observer, after they are written.
            AgentEvent::InputMessage(_)
            | AgentEvent::MessageComplete(_)
            | AgentEvent::ToolComplete(_)
            | AgentEvent::RunComplete(_)
            // Streaming noise with no wire shape.
            | AgentEvent::MessageStart(_)
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

/// Publishes an agent's durable history to its broadcast.
///
/// Invoked from `AgentActor::on_events_persisted`, so every frame here
/// describes something already journaled and folded. Best-effort: a send with
/// no subscribers is a no-op, which is the normal case for a session nobody is
/// watching.
pub struct BroadcastObserver {
    pub frames: broadcast::Sender<AgentFrame>,
}

impl AgentObserver for BroadcastObserver {
    fn publish(&self, event: &AgentDomainEvent, _state: &AgentState) {
        // Derived from the event, never from `state.messages.last()`: the hook
        // hands us the state after the *whole batch* folded, so the last message
        // belongs to the last event, not to this one.
        if let Some(frame) = agent_frame(event) {
            let _ = self.frames.send(frame);
        }
    }
}

/// Map one durable agent event onto its broadcast frame (`None` = not surfaced).
///
/// The three transcript-bearing events all become `Appended`, mirroring
/// `AgentActor::apply_event`, which pushes exactly one message for each — that
/// correspondence is what lets a client accumulate appends and get the same
/// transcript `/history` would hand it.
fn agent_frame(event: &AgentDomainEvent) -> Option<AgentFrame> {
    match event {
        AgentDomainEvent::InputMessage { message }
        | AgentDomainEvent::MessageComplete { message } => {
            let mut message = message.clone();
            crate::wire_redact::strip_message_signature(&mut message);
            Some(AgentFrame::Appended { message })
        }
        AgentDomainEvent::ToolComplete {
            tool_call_id,
            output,
            is_error,
            at_ms,
        } => Some(AgentFrame::Appended {
            message: Message::tool_result(tool_call_id, output, *is_error, *at_ms),
        }),
        AgentDomainEvent::RunComplete {
            usage,
            iterations,
            at_ms,
            ..
        } => Some(AgentFrame::TurnCompleted {
            iterations: *iterations,
            usage: usage.clone(),
            at_ms: *at_ms,
        }),
        AgentDomainEvent::TaskListChanged { snapshot, .. } => Some(AgentFrame::TaskListChanged {
            tasks: snapshot.tasks().to_vec(),
        }),
        AgentDomainEvent::RunCancelled { .. }
        | AgentDomainEvent::TimerArmed { .. }
        | AgentDomainEvent::TimerCancelled { .. }
        | AgentDomainEvent::TimerFired { .. }
        | AgentDomainEvent::Parked { .. } => None,
    }
}

/// Map a broadcast frame onto the wire shape the agent stream sends.
pub(crate) fn wire_agent_frame(frame: AgentFrame) -> AgentStreamEvent {
    match frame {
        AgentFrame::Appended { message } => AgentStreamEvent::Appended(AppendedEvent { message }),
        AgentFrame::Delta { text } => AgentStreamEvent::Delta(DeltaEvent { text }),
        AgentFrame::ToolStart { tool_call_id, name } => {
            AgentStreamEvent::ToolStart(ToolStartEvent { tool_call_id, name })
        }
        AgentFrame::TurnCompleted {
            iterations,
            usage,
            at_ms,
        } => AgentStreamEvent::TurnCompleted(TurnCompletedEvent {
            iterations,
            usage,
            at_ms,
        }),
        AgentFrame::TaskListChanged { tasks } => AgentStreamEvent::TaskListChanged(TaskListEvent {
            tasks: tasks.iter().map(wire_task).collect(),
        }),
    }
}

/// The frame a lagging subscriber gets instead of a silent gap.
pub(crate) fn resync_frame() -> AgentStreamEvent {
    AgentStreamEvent::Resync(ResyncEvent {})
}

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

    /// Every transcript-bearing event becomes exactly one `Appended`, matching
    /// `AgentActor::apply_event`, which pushes exactly one message for each.
    #[test]
    fn transcript_events_all_become_one_append() {
        let msg = horsie_models::agent::Message::user("m1", "hello", 0);
        match agent_frame(&AgentDomainEvent::InputMessage {
            message: msg.clone(),
        }) {
            Some(AgentFrame::Appended { message }) => assert_eq!(message.id, "m1"),
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
            Some(AgentFrame::Appended { message }) => {
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

    #[test]
    fn a_transcript_message_carries_its_own_stamps() {
        // `/history` is answered from agent state, so the stamps must live on
        // the message itself rather than beside it on the event.
        let mut message = horsie_models::agent::Message::user("m1", "hello", 1_700_000_000_001);
        message.started_at_ms = Some(1_700_000_000_000);
        match agent_frame(&AgentDomainEvent::MessageComplete { message }) {
            Some(AgentFrame::Appended { message }) => {
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
            AgentFrame::Appended { message } => match &message.parts[0] {
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
