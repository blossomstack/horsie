//! The two identities the runner tree is addressed by.
//!
//! One namespace, two roles. An [`AgentId`] names one LLM loop; a [`RunnerId`]
//! names one unit of work. The pair replaces five parallel enums (`AgentKey`,
//! `SessionAgentKind`, `SubAgentParent`, `ForkParent`, `TreeOwner`) that each
//! re-encoded "which agent, of which kind, under whom" — what an id *is* now
//! lives in the state entry it keys, so every routing question is one lookup
//! instead of an ordered probe of three registries.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One LLM loop a session hosts.
///
/// The main agent's id is the session's own id — already true on the wire
/// (its journal is keyed by the session uuid) — so `"main"` is a spelling
/// resolved at the API boundary, never a type-level special case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub Uuid);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One unit of work a session hosts: the conversation it is, a fork of one, a
/// delegated task, or a workflow run.
///
/// A runner that wraps a single agent shares that agent's id — the runner *is*
/// how that agent is run, and a second uuid would be a second name for one
/// thing. A workflow runner owns many step agents and none of their ids: the
/// session-root run is keyed by the session id (no main agent exists to
/// collide with), and a nested run gets a fresh uuid minted where its creation
/// is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerId(pub Uuid);

impl RunnerId {
    /// The runner an agent of the same id belongs to. How a Main, Sub or Fork
    /// runner is found from its agent; a step agent misses here and is found
    /// through its run's log instead.
    #[must_use]
    pub fn of_agent(agent: AgentId) -> Self {
        Self(agent.0)
    }
}

impl std::fmt::Display for RunnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
