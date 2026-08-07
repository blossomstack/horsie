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
pub mod mode;
pub mod orchestrator;
pub mod session_actor;
pub mod spawn_tool;
pub mod spec;
pub mod subagents;
pub mod supervisor;
pub mod title_tool;
pub mod workflow;

/// Where each of a session's agents has got to, for readers to wait on.
///
/// **Owned by the supervisor, not by the session.** An idle session unloads,
/// and a reader parked on one of these must not be disconnected by that: the
/// alternative is a loop, because a disconnected browser reconnects, a
/// reconnect loads the session, and a loaded session goes idle again. The
/// channel outliving the actor is what breaks that cycle — a reader simply
/// waits, and hears from the session the next time something actually wakes it.
///
/// This is the same shape the old session-frame channel had, and for the same
/// reason; it is per-agent now because that is what a reader subscribes to.
///
/// A `watch` carries only `(tail_seq, delta_count)`. It keeps the latest value
/// and overwrites, so a slow reader cannot fall behind it and there is nothing
/// to overflow — the data itself is read from the agent's state.
/// Keyed by the *wire* agent id — `"main"`, or a subagent/step uuid — not by
/// `AgentKey`. The supervisor answers a subscribe without loading the session,
/// and telling a `Sub` uuid from a `Step` uuid needs session state it
/// deliberately does not read. A uuid is one or the other and never both, so
/// the wire id is already unambiguous.
/// `(tail_seq, delta_count)` — how far an agent has got.
pub type Position = (u64, usize);

/// One agent's channel. `Arc` because the supervisor and the agent both hold
/// it, and the supervisor's copy is what keeps it alive across an offload.
pub type PositionSender = std::sync::Arc<tokio::sync::watch::Sender<Position>>;

type PositionMap = std::collections::HashMap<String, PositionSender>;

#[derive(Clone, Default)]
pub struct Positions(std::sync::Arc<std::sync::Mutex<PositionMap>>);

impl Positions {
    /// This agent's channel, created on first use.
    #[must_use]
    pub fn for_agent(&self, id: &str) -> std::sync::Arc<tokio::sync::watch::Sender<(u64, usize)>> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::watch::Sender::new((0, 0))))
            .clone()
    }

    /// Whether anyone is still waiting on any of these, so the supervisor can
    /// drop the registry of a session nobody is watching.
    #[must_use]
    pub fn watched(&self) -> bool {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.values().any(|tx| tx.receiver_count() > 0)
    }
}

impl std::fmt::Debug for Positions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Positions")
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
