//! Interactive sessions: event-sourced actors on the shared `horsie-actor` core.
//!
//! `SessionSupervisor` (journal `session-supervisor/<account>`) owns the
//! registry of which sessions exist; a `SessionActor` (journal `session/<id>`)
//! is one of them, and hosts an `AgentActor` per agent (journal `agent/<id>`).
//! The two are separate clustered types rather than parent and child, so each
//! is placed on its own — see [`addressing`]. Recovery is lazy: a journal
//! replays when something addresses the actor it belongs to, and runtimes
//! respawn only on user action.

use serde::{Deserialize, Serialize};

pub mod addressing;
pub mod ask_tool;
pub mod builder;
pub mod clock;
pub mod events;
pub mod forks;
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

/// One agent's channel. `Arc` because the account's registry and the agent both
/// hold it, and the registry's copy is what keeps it alive across an offload.
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
/// **Outlives the session actor, deliberately.** An idle session unloads, and a
/// reader waiting on one of these must not be cut off by that: the alternative
/// is a loop, because a disconnected browser reconnects, a reconnect loads the
/// session, and a loaded session goes idle again. The channel outliving the
/// actor is what breaks that cycle — a reader simply waits, and hears from the
/// session the next time something actually wakes it.
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

/// Every session's revision channels, and the account's own list counter.
///
/// **Node-local, and that is the constraint everything here is shaped by.** A
/// `watch` channel is a pointer into this process, so a reader served by
/// another host polls *that* host's copy of the number. What travels between
/// hosts is therefore always the number and never a handle to it.
///
/// Which is why the two counters here reach a reader by different routes, and
/// the difference is not arbitrary:
///
/// - the **list** counter is moved by the supervisor and read by the supervisor,
///   so a reader on any host reaches it with one `ask` and this copy is the
///   only copy that matters;
/// - an **agent** counter is moved by the *session* and read by the
///   *supervisor*, which are placed independently — so the session publishes
///   the value and whichever host answers a reader mirrors it into its own copy
///   here.
///
/// An earlier version of this comment said keeping the map on the account's
/// bundle solved that second case, because "a map on either actor would be
/// invisible to the other". The bundle is node-local too, so it was only ever
/// true while both actors happened to share a process.
#[derive(Default)]
pub struct SessionRevisions {
    sessions: std::sync::Mutex<std::collections::HashMap<String, Revisions>>,
    /// How many times this account's session list has changed — a status, a
    /// title, or a fork set. One counter for the whole list rather than one per
    /// session: a reader of the list re-reads the list, so knowing *that* it
    /// moved is all the counter has to carry.
    list: RevisionSender,
}

impl SessionRevisions {
    /// The account's session-list counter, for a reader to wait on.
    #[must_use]
    pub fn list(&self) -> RevisionSender {
        self.list.clone()
    }

    /// Note that the session list changed.
    ///
    /// Absolute rather than a delta, which is what makes a missed observation
    /// harmless: whoever looks next sees the current value and re-reads the
    /// list, rather than needing every step in between.
    pub fn bump_list(&self) {
        self.list.send_modify(|v| *v += 1);
    }
    /// One session's channels, created on first use.
    #[must_use]
    pub fn of(&self, session: &str) -> Revisions {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.to_string())
            .or_default()
            .clone()
    }

    /// Drop a session's channels unless a reader is still interested.
    ///
    /// Called when a session unloads or is deleted. Keeping a watched one is
    /// the whole point: an unloaded session has nothing to say until something
    /// reloads it, and ending the stream would only make the client reconnect
    /// and reload it.
    pub fn release(&self, session: &str) {
        let mut map = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.get(session).is_none_or(|p| !p.watched()) {
            map.remove(session);
        }
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
#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The list counter is absolute, not a delta.
    ///
    /// That is the property that makes a missed observation harmless: a reader
    /// that looks after two changes sees one number it can compare, re-reads
    /// the list, and is correct — it never needs the steps in between. Every
    /// decision about how this value reaches another host rests on it.
    #[test]
    fn the_list_revision_is_absolute_and_moves_on_every_change() {
        let revisions = SessionRevisions::default();
        let start = *revisions.list().borrow();
        revisions.bump_list();
        revisions.bump_list();
        assert_eq!(*revisions.list().borrow(), start + 2);
    }

    /// One counter for the whole list, not one per session.
    ///
    /// A reader of the list re-reads the list, so knowing *that* it moved is
    /// all this has to carry — and a counter per session would be a map to
    /// keep in step with the sessions themselves.
    #[test]
    fn every_session_shares_the_one_list_revision() {
        let revisions = SessionRevisions::default();
        let before = *revisions.list().borrow();
        revisions
            .of("session-a")
            .for_agent("main")
            .send_modify(|v| *v += 1);
        assert_eq!(
            *revisions.list().borrow(),
            before,
            "an agent moving is not the list changing"
        );
        revisions.bump_list();
        assert_eq!(*revisions.list().borrow(), before + 1);
    }
}
