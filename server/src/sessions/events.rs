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

/// A session's own journal, decoded — **tests only**.
///
/// The fold answers "where is this session now"; this answers "what happened to
/// it", which is what a test asserting on a *transition* needs. Status alone
/// cannot distinguish a turn that never began from one that began and finished
/// between two polls.
#[cfg(test)]
pub(in crate::sessions) async fn session_events(
    journal: &std::sync::Arc<dyn horsie_actor::Journal>,
    session_id: uuid::Uuid,
) -> Vec<crate::sessions::session_actor::SessionDomainEvent> {
    use futures_util::StreamExt;

    let pid = crate::sessions::session_actor::SessionActor::persistence_id_for(session_id);
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection of a journal, with no actor running to ask"
    )]
    let mut stream = journal.replay(&pid, 0).await;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let Ok((_seq, bytes)) = item else { break };
        if let Ok(event) = serde_json::from_slice(&bytes) {
            out.push(event);
        }
    }
    out
}

fn wire_task_status(status: AgentTaskStatus) -> WireTaskStatus {
    match status {
        AgentTaskStatus::Pending => WireTaskStatus::Pending,
        AgentTaskStatus::InProgress => WireTaskStatus::InProgress,
        AgentTaskStatus::Completed => WireTaskStatus::Completed,
    }
}
