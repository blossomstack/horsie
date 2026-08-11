//! Interactive sessions: event-sourced actors on the shared `horsie-actor` core.
//!
//! `SessionSupervisor` (journal `session-supervisor/main`) owns the registry and
//! one `SessionActor` child per live session (journal `session/<id>`); each
//! session hosts a reused `AgentActor` (journal `agent/<id>`). Recovery is lazy:
//! journals replay at startup, runtimes respawn only on user action.

pub mod ask_tool;
pub mod builder;
pub mod clock;
pub mod events;
pub mod lifecycle_routing;
pub mod orchestrator;
pub mod session_actor;
pub mod spawn_tool;
pub mod spec;
pub mod subagents;
pub mod supervisor;
pub mod title_tool;
pub mod workflow;

/// How many times an agent has moved. Opaque: a reader compares it with the
/// last one it saw and re-reads when they differ, and that is all it means.
///
/// A counter rather than the `(tail_seq, delta_count)` pair this used to carry.
/// A reader now compares two values instead of holding a channel, and that pair
/// does not survive the comparison: an agent's first entry lands at sequence
/// zero with no deltas, which is bit-for-bit the value a reader starts from, so
/// the one thing a stream must never miss would have looked like no news at all.
pub type Revision = u64;

/// One agent's channel. `Arc` because the supervisor and the agent both hold
/// it, and the supervisor's copy is what keeps it alive across an offload.
pub type RevisionSender = std::sync::Arc<tokio::sync::watch::Sender<Revision>>;

type RevisionMap = std::collections::HashMap<String, RevisionSender>;

#[derive(Default)]
struct Registry {
    agents: RevisionMap,
    /// When a reader last asked about any of these. See `watched`.
    polled: Option<std::time::Instant>,
}

/// How many times each of a session's agents has moved, for readers to wait on.
///
/// **Owned by the supervisor, not by the session.** An idle session unloads,
/// and a reader waiting on one of these must not be cut off by that: the
/// alternative is a loop, because a disconnected browser reconnects, a
/// reconnect loads the session, and a loaded session goes idle again. The
/// channel outliving the actor is what breaks that cycle — a reader simply
/// waits, and hears from the session the next time something actually wakes it.
///
/// This is the same shape the old session-frame channel had, and for the same
/// reason; it is per-agent now because that is what a reader waits on.
///
/// A `watch` carries only the counter. It keeps the latest value and
/// overwrites, so a slow reader cannot fall behind it and there is nothing to
/// overflow — what actually happened is read from the agent's state.
///
/// Keyed by the *wire* agent id — `"main"`, or a subagent/step uuid — not by
/// `AgentKey`. The supervisor answers without loading the session, and telling
/// a `Sub` uuid from a `Step` uuid needs session state it deliberately does not
/// read. A uuid is one or the other and never both, so the wire id is already
/// unambiguous.
#[derive(Clone, Default)]
pub struct Revisions(std::sync::Arc<std::sync::Mutex<Registry>>);

impl Revisions {
    /// This agent's channel, created on first use.
    #[must_use]
    pub fn for_agent(&self, id: &str) -> RevisionSender {
        let mut reg = self.0.lock().unwrap_or_else(|e| e.into_inner());
        reg.agents
            .entry(id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::watch::Sender::new(0)))
            .clone()
    }

    /// Note that a reader just asked where an agent has got to.
    pub fn touch(&self) {
        let mut reg = self.0.lock().unwrap_or_else(|e| e.into_inner());
        reg.polled = Some(std::time::Instant::now());
    }

    /// Whether a reader is still interested, so the supervisor can drop the
    /// registry of a session nobody is watching.
    ///
    /// Recency, not a live receiver count. A reader holds a receiver only while
    /// its poll is waiting, and lets go for the moment it spends reading the log
    /// — so counting receivers would let an offload landing in that moment throw
    /// the registry away. The counter would restart at zero, the reader would
    /// see a change that did not happen, and its read would load the session
    /// again: the reload loop this registry exists to prevent, on a timer.
    #[must_use]
    pub fn watched(&self) -> bool {
        let reg = self.0.lock().unwrap_or_else(|e| e.into_inner());
        reg.polled.is_some_and(|at| at.elapsed() < WATCH_RETENTION)
            || reg.agents.values().any(|tx| tx.receiver_count() > 0)
    }
}

/// How long after a reader's last question its channels are kept. Comfortably
/// longer than one poll window, so an active reader always renews in time and a
/// departed one lapses shortly after.
const WATCH_RETENTION: std::time::Duration = std::time::Duration::from_secs(120);

impl std::fmt::Debug for Revisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Revisions")
    }
}

/// Why a message could not be accepted. There is no "busy" here by design: a
/// turn in flight queues the message rather than rejecting it.
#[derive(Debug, thiserror::Error)]
pub enum UserMessageError {
    #[error("session not found")]
    NotFound,
    #[error("session is unrecoverable: {0}")]
    Unrecoverable(String),
    /// This session kind takes no messages — a workflow run works from its
    /// definition. Comes from `Orchestrator::accepts`, so the rule lives in one
    /// place rather than in a handler guard.
    #[error("{0}")]
    Rejected(String),
}
