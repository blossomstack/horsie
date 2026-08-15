//! A session hosts runners; a runner owns one agent role and the capabilities
//! its agents are equipped with.
//!
//! Nothing in this module performs anything. A runner and a capability both
//! answer with `(events, actions)` — the events are what to journal, the
//! actions are what the session should do — which is what makes every decision
//! here testable against a hand-built state with no actor, no runtime and no
//! journal.
//!
//! That purity is also why there is no `run()` or `init()`. Starting work is
//! [`Runner::actions`], a pure function of the folded state, so creation and
//! recovery take the same path: a `Pending` runner with no agents starts its
//! first agent whether that state arrived a millisecond ago or from a journal
//! replayed after a restart. A `run()` that fired once would need a second
//! entry path for recovery, which either double-starts every agent or has to
//! be suppressed — and the suppression is where the bugs live.
//!
//! Three rules hold across everything below, and each replaces a way the
//! previous shape went wrong:
//!
//! 1. A runner writes only its own slice. Cross-runner facts arrive as calls
//!    ([`AgentLifecycle::on_agent_ended`], a child's outcome), never as reads.
//! 2. A runner returns events; the fold is the only writer. Nothing mutates
//!    state directly, so replay and live operation cannot diverge.
//! 3. A runner impl holds no fields. Everything it knows is in the state
//!    handed to it — a field would not survive a reload, and worse, would
//!    silently differ from what the log says.
#![allow(dead_code)] // Phase A: built and tested here, wired into the actor in Phase B.

pub mod capabilities;
pub mod ids;
pub mod state;

pub use ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
pub use state::{RunnerRecord, SessionState};

use serde::{Deserialize, Serialize};

/// A runner's own slice.
///
/// One arm per kind, and the session never matches on it: it hands the whole
/// value back to the impl that owns it. A closed enum rather than an opaque
/// blob so the journal stays typed and a shape change fails to compile where
/// it should.
///
/// The payloads land as their runners do, in the tasks that add them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerState {
    Conversation,
    SubAgent,
    Workflow,
    Runtime,
}

impl RunnerState {
    #[must_use]
    pub fn for_kind(kind: RunnerKind) -> Self {
        match kind {
            RunnerKind::Conversation => Self::Conversation,
            RunnerKind::SubAgent => Self::SubAgent,
            RunnerKind::Workflow => Self::Workflow,
            RunnerKind::Runtime => Self::Runtime,
        }
    }
}
