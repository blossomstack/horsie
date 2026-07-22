//! Event plumbing for session SSE streams.
//!
//! Durable coarse events are read from the agent journal with stable sequence
//! ids (the SSE cursor space — interactive agents never compact, so ids are
//! exact journal positions forever). Ephemeral deltas ride the live broadcast
//! without ids.

use crate::sessions::SessionFrame;
use crate::sessions::session_actor::{SessionActor, SessionDomainEvent, SessionState};
use async_trait::async_trait;
use futures_util::StreamExt;
use horsie_actor::{EventSourcedActor, Journal};
use horsie_agentcore::{AgentEvent, EventSink, EventSinkError};
use horsie_models::session::{
    MessageEvent, SessionEvent, TaskItem, TaskListEvent, TaskStatus as WireTaskStatus,
    ToolOutputEvent, TurnCompletedEvent,
};
use horsie_workflow::{AgentActor, AgentDomainEvent, TaskStatus as AgentTaskStatus};
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

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

/// A coarse event replayed from the agent journal, with its stable sequence id.
#[derive(Debug, Clone)]
pub struct StampedEvent {
    pub seq: u64,
    pub event: SessionEvent,
}

/// Replay the session's agent journal after `after_seq`, mapping each journaled
/// [`AgentDomainEvent`] to its wire [`SessionEvent`]. Every journal entry
/// advances the sequence counter — including entries that produce no frame
/// (cancellations, timer events) — so ids match journal positions exactly.
/// Interactive agents never compact, so replaying from 0 with our own counter
/// is exact.
/// The current journal head (number of persisted entries) for a session's agent.
/// Used by the SSE `live` mode to begin streaming *after* everything already in
/// the journal, so a paginating client that backfills via `/history` does not
/// also receive the whole transcript over SSE. Counts entries without decoding
/// them.
pub async fn journal_head_seq(journal: &Arc<dyn Journal>, session_id: Uuid) -> u64 {
    let pid = AgentActor::persistence_id_for(session_id);
    let mut seq = 0u64;
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        if item.is_err() {
            break;
        }
        seq += 1;
    }
    seq
}

pub async fn replay_session_events(
    journal: &Arc<dyn Journal>,
    session_id: Uuid,
    after_seq: u64,
) -> Vec<StampedEvent> {
    let pid = AgentActor::persistence_id_for(session_id);
    let mut out = Vec::new();
    let mut seq = 0u64;
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        let bytes = match item {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(%pid, error = %e, "journal replay error; truncating SSE history");
                break;
            }
        };
        seq += 1;
        if seq <= after_seq {
            continue;
        }
        match serde_json::from_slice::<AgentDomainEvent>(&bytes) {
            Ok(event) => {
                if let Some(wire) = wire_event(event) {
                    out.push(StampedEvent { seq, event: wire });
                }
            }
            Err(e) => {
                tracing::warn!(%pid, seq, error = %e, "undecodable journal event; skipping");
            }
        }
    }
    out
}

/// Map one journaled agent event onto its wire shape (`None` = not surfaced).
fn wire_event(event: AgentDomainEvent) -> Option<SessionEvent> {
    match event {
        AgentDomainEvent::InputMessage { message }
        | AgentDomainEvent::MessageComplete { message } => {
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
        AgentDomainEvent::RunComplete { usage, iterations } => {
            Some(SessionEvent::TurnCompleted(TurnCompletedEvent {
                iterations,
                usage,
            }))
        }
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

/// Fold a session's own journal into its [`SessionState`] (durable truth for
/// `pending_question` / `last_error` on the detail endpoint). Session actors
/// never snapshot, so replaying from 0 sees the full log.
pub async fn fold_session_state(journal: &Arc<dyn Journal>, session_id: Uuid) -> SessionState {
    let pid = SessionActor::persistence_id_for(session_id);
    let mut state = SessionState::default();
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        let Ok(bytes) = item else { break };
        if let Ok(event) = serde_json::from_slice::<SessionDomainEvent>(&bytes) {
            state = SessionActor::apply_event(state, event);
        }
    }
    state
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
    use horsie_actor::InMemoryJournal;

    #[tokio::test]
    async fn replay_maps_and_stamps_sequential_ids() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let sid = Uuid::new_v4();
        let pid = AgentActor::persistence_id_for(sid);
        let msg = horsie_models::agent::Message::user("m1", "hello");
        let events = vec![
            serde_json::to_vec(&AgentDomainEvent::InputMessage {
                message: msg.clone(),
            })
            .unwrap(),
            // No frame, still consumes a sequence number.
            serde_json::to_vec(&AgentDomainEvent::RunCancelled).unwrap(),
            serde_json::to_vec(&AgentDomainEvent::ToolComplete {
                tool_call_id: "tc".into(),
                output: "ok".into(),
                is_error: false,
            })
            .unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();

        let all = replay_session_events(&journal, sid, 0).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 3); // RunCancelled consumed seq 2
        match &all[0].event {
            SessionEvent::Message(m) => assert_eq!(m.message.id, "m1"),
            other => panic!("expected Message, got {other:?}"),
        }

        let after = replay_session_events(&journal, sid, 1).await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq, 3);
    }

    #[tokio::test]
    async fn task_list_changed_maps_to_wire_event() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let sid = Uuid::new_v4();
        let pid = AgentActor::persistence_id_for(sid);
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
        let events =
            vec![serde_json::to_vec(&AgentDomainEvent::TaskListChanged { snapshot }).unwrap()];
        journal.persist(&pid, &events).await.unwrap();

        let all = replay_session_events(&journal, sid, 0).await;
        assert_eq!(all.len(), 1);
        match &all[0].event {
            SessionEvent::TaskListChanged(e) => {
                assert_eq!(e.tasks.len(), 2);
                assert_eq!(e.tasks[0].id, 1);
                assert_eq!(e.tasks[0].content, "a");
                assert_eq!(e.tasks[0].status, WireTaskStatus::Completed);
                assert_eq!(e.tasks[1].status, WireTaskStatus::Pending);
            }
            other => panic!("expected TaskListChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fold_session_state_reads_pending_question() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let sid = Uuid::new_v4();
        let pid = SessionActor::persistence_id_for(sid);
        let events = vec![
            serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
            serde_json::to_vec(&SessionDomainEvent::Asked {
                tool_call_id: Some("tc".into()),
                question: "which one?".into(),
            })
            .unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();
        let state = fold_session_state(&journal, sid).await;
        assert_eq!(state.pending_question.as_deref(), Some("which one?"));
        assert_eq!(state.pending_ask.as_deref(), Some("tc"));
    }
}
