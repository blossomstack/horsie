//! The session's forks: which conversation each branched from, and where each
//! one has got to. Pure data — the session actor folds its journal events
//! through these methods, so live operation and recovery follow one path.
//!
//! Deliberately not a [`SubAgentTree`](crate::sessions::subagents::SubAgentTree).
//! That structure's whole vocabulary — `notified`, `TreeOwner`,
//! `owed_deliveries` — exists to guarantee a parent eventually receives a
//! child's result. A fork owes nobody one, so putting it there would mean
//! carrying fields that must always be inert and could always be read wrong.

use crate::sessions::session_actor::AgentStatus;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The agent a fork was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ForkParent {
    /// The session's main agent.
    Main,
    /// Another fork. Forks nest arbitrarily — a person types `/fork`, so there
    /// is no runaway to bound the way `MAX_SUBAGENT_DEPTH` bounds a machine
    /// that can spawn in a loop.
    Fork(Uuid),
}

/// How a fork's history was seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkMode {
    /// `/fork` — the source's log, copied and scrubbed.
    Copy,
    /// `/summary-n-fork` — a summary of the source, produced out of band.
    Summary,
}

impl ForkMode {
    /// The wire spelling, and what a lifecycle entry carries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Summary => "summary",
        }
    }
}

/// One fork.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRecord {
    pub parent: ForkParent,
    /// The source agent's log seq this fork was taken at — the branch point.
    pub source_seq: u64,
    pub mode: ForkMode,
    /// What the fork was created to do — the message typed after `/fork`.
    ///
    /// Durable here, not merely queued on the agent, because a fork abandoned
    /// mid-seed is re-seeded from this record and would otherwise come back
    /// with nothing to do.
    pub message: String,
    /// What the fork has named itself, once it has. `None` until then; a client
    /// falls back to the mode and the moment.
    pub title: Option<String>,
    pub status: AgentStatus,
    pub created_at_ms: u64,
    /// When this fork last did anything — the moment of its most recent status
    /// change, which is the end of its last turn once it is idle again.
    ///
    /// A conversation has no *end*, which is why `fork_entry` reports none. But
    /// a reader looking at a session's shape still needs to know how far along
    /// a fork got, and "still going, forever" is not that. Zero until the fork
    /// has moved at all. `#[serde(default)]` so pre-stamp journal rows load.
    #[serde(default)]
    pub last_activity_ms: u64,
}

/// Every fork a session holds, keyed by agent id. Iteration is uuid order,
/// which is stable — a client sorts for display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForkRoster {
    forks: BTreeMap<Uuid, ForkRecord>,
}

impl ForkRoster {
    pub fn apply_created(
        &mut self,
        id: Uuid,
        parent: ForkParent,
        source_seq: u64,
        mode: ForkMode,
        message: String,
        at_ms: u64,
    ) {
        self.forks.insert(
            id,
            ForkRecord {
                parent,
                source_seq,
                mode,
                message,
                title: None,
                // Nothing may run until the seed lands — the same status a
                // session uses while its runtime is built, for the same reason,
                // and the reason a fork found in it at load is safe to re-seed:
                // it is precisely the state in which no turn has run.
                status: AgentStatus::Provisioning,
                created_at_ms: at_ms,
                last_activity_ms: at_ms,
            },
        );
    }

    /// The seed is durable, so the fork may run.
    pub fn apply_seeded(&mut self, id: Uuid) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.status = AgentStatus::Idle;
        }
    }

    pub fn apply_titled(&mut self, id: Uuid, title: String) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.title = Some(title);
        }
    }

    /// A fork moved. `at_ms` is the event's own stamp, so a replay reproduces
    /// exactly the activity times a live run recorded.
    pub fn apply_status(&mut self, id: Uuid, status: AgentStatus, at_ms: u64) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.status = status;
            rec.last_activity_ms = at_ms;
        }
    }

    pub fn apply_deleted(&mut self, id: Uuid) {
        self.forks.remove(&id);
    }

    #[must_use]
    pub fn get(&self, id: Uuid) -> Option<&ForkRecord> {
        self.forks.get(&id)
    }

    #[must_use]
    pub fn contains(&self, id: Uuid) -> bool {
        self.forks.contains_key(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Uuid, &ForkRecord)> {
        self.forks.iter()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forks.is_empty()
    }

    /// Forks whose seed never landed.
    ///
    /// Re-seeded at load: seeding is session-owned work with no journal of its
    /// own, so — unlike a turn, which the agent reports as interrupted from its
    /// own recovery — nothing else can finish one a dead process abandoned.
    #[must_use]
    pub fn seeding(&self) -> Vec<Uuid> {
        self.forks
            .iter()
            .filter(|(_, r)| matches!(r.status, AgentStatus::Provisioning))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether any fork is mid-seed, so the session must not unload out from
    /// under an in-flight summariser call.
    #[must_use]
    pub fn has_seeding(&self) -> bool {
        self.forks
            .values()
            .any(|r| matches!(r.status, AgentStatus::Provisioning))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn a_created_fork_starts_provisioning_and_unnamed() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            42,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        let rec = r.get(id(1)).unwrap();
        assert_eq!(rec.message, "go");
        assert_eq!(rec.parent, ForkParent::Main);
        assert_eq!(rec.source_seq, 42);
        assert_eq!(rec.mode, ForkMode::Copy);
        assert_eq!(rec.title, None);
        assert_eq!(rec.status, AgentStatus::Provisioning);
        assert_eq!(rec.created_at_ms, 1_000);
    }

    /// Seeding is what ends the provisioning window: until it lands there is
    /// nothing for the fork to run against.
    #[test]
    fn seeding_moves_a_fork_to_idle() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Summary,
            "go".into(),
            1_000,
        );
        assert!(
            r.has_seeding(),
            "a fork awaiting its seed keeps the session loaded"
        );
        r.apply_seeded(id(1));
        assert_eq!(r.get(id(1)).unwrap().status, AgentStatus::Idle);
        assert!(!r.has_seeding());
        assert!(r.seeding().is_empty());
    }

    #[test]
    fn a_fork_names_itself() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        r.apply_titled(id(1), "Try the other migration".to_string());
        assert_eq!(
            r.get(id(1)).unwrap().title.as_deref(),
            Some("Try the other migration")
        );
    }

    #[test]
    fn a_fork_of_a_fork_records_its_parent() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        r.apply_created(
            id(2),
            ForkParent::Fork(id(1)),
            7,
            ForkMode::Copy,
            "go".into(),
            2_000,
        );
        assert_eq!(r.get(id(2)).unwrap().parent, ForkParent::Fork(id(1)));
    }

    #[test]
    fn deleting_a_fork_leaves_its_siblings() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        r.apply_created(
            id(2),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            2_000,
        );
        r.apply_deleted(id(2));
        assert!(r.contains(id(1)));
        assert!(!r.contains(id(2)));
    }

    /// Deleting a fork orphans nothing: a child fork keeps its own transcript,
    /// and a parent id that no longer resolves renders at the top level.
    #[test]
    fn deleting_a_parent_fork_leaves_its_child() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        r.apply_created(
            id(2),
            ForkParent::Fork(id(1)),
            0,
            ForkMode::Copy,
            "go".into(),
            2_000,
        );
        r.apply_deleted(id(1));
        assert!(r.contains(id(2)), "a child fork is its own conversation");
    }

    /// A fold applied to an id that is gone must not resurrect it: events for a
    /// deleted fork can still be in flight when the delete lands.
    #[test]
    fn events_for_a_deleted_fork_are_ignored() {
        let mut r = ForkRoster::default();
        r.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Copy,
            "go".into(),
            1_000,
        );
        r.apply_deleted(id(1));
        r.apply_seeded(id(1));
        r.apply_titled(id(1), "ghost".to_string());
        r.apply_status(id(1), AgentStatus::Running, 2_000);
        assert!(!r.contains(id(1)));
    }

    /// Pre-fork journal rows have no `forks` key at all.
    #[test]
    fn an_absent_roster_deserializes_empty() {
        let r: ForkRoster = serde_json::from_str("{}").unwrap();
        assert_eq!(r.iter().count(), 0);
        assert!(r.is_empty());
    }
}
