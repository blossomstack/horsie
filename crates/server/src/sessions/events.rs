//! Task-list wire mapping, and the session-state fold used by tests.
//!
//! The broadcast plumbing that used to live here — an ephemeral event sink, a
//! post-persist observer, and the frame unions both fed — is gone. Durable
//! entries are read from the agent's own state through `AgentCommand::ReadLog`,
//! and deltas reach the agent through its mailbox, so there is no second copy
//! of the transcript in flight to keep ordered against the first.

use crate::agent_loop::TaskStatus as AgentTaskStatus;
use horsie_models::agent::{TaskItem, TaskStatus as WireTaskStatus};

pub(crate) fn wire_task(t: &crate::agent_loop::TaskRecord) -> TaskItem {
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
            serde_json::from_slice::<crate::sessions::session_actor::SessionEvent>(&bytes)
        {
            state = crate::sessions::session_actor::SessionActor::apply_event(state, event);
        }
    }
    state
}

/// One agent's folded state — **tests only**.
///
/// A step's timers, asks and queue live on the *agent's* journal, not the
/// session's, so a test asserting on any of them has to read this one.
#[cfg(test)]
pub(in crate::sessions) async fn fold_agent_state(
    journal: &std::sync::Arc<dyn horsie_actor::Journal>,
    agent_id: uuid::Uuid,
) -> crate::agent_loop::AgentState {
    use futures_util::StreamExt;
    use horsie_actor::EventSourcedActor;

    let pid = crate::agent_loop::AgentActor::persistence_id_for(agent_id);
    // From the snapshot, not from zero: an agent snapshots at every park, and a
    // snapshot compacts the events behind it — so replaying from 0 answers
    // "nothing ever happened" for exactly the agents a test most wants to read.
    // From `seq`, not `seq + 1`: a snapshot is recorded at the sequence number
    // of the first event it does *not* include, which is what the actor's own
    // recovery replays from. Skipping one past it silently drops exactly one
    // event after every snapshot — which for a parked agent is the very next
    // thing that happened to it.
    let (mut state, from) = match journal.latest_snapshot(&pid).await {
        Ok(Some((bytes, seq))) => (
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| crate::agent_loop::AgentActor::initial_state()),
            seq,
        ),
        _ => (crate::agent_loop::AgentActor::initial_state(), 0),
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection of a journal, with no actor running to ask"
    )]
    let mut stream = journal.replay(&pid, from).await;
    while let Some(item) = stream.next().await {
        let Ok((_seq, bytes)) = item else { break };
        if let Ok(event) = serde_json::from_slice::<crate::agent_loop::AgentDomainEvent>(&bytes) {
            state = crate::agent_loop::AgentActor::apply_event(state, event);
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
) -> Vec<crate::sessions::session_actor::SessionEvent> {
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
