//! What a runner decides: the events it wants journaled, the work it wants
//! started, and the repairs it wants after a crash.
//!
//! Everything here is a *description*. The session actor performs it — spawns
//! the agent, enqueues the delivery, persists the events — and folds the
//! result. Every field is what the actor needs to do that without re-deriving
//! a decision.

use horsie_models::agent::SubAgentResultPart;
use serde_json::Value;

use super::event::SessionEvent;
use super::ids::{AgentId, RunnerId};

/// What a runner made of one of its agents' turn ends.
#[derive(Debug)]
pub(crate) struct OutcomeDecision {
    /// The events to journal, in order.
    pub events: Vec<SessionEvent>,
    /// Whether this end is a boundary the actor should drain — deliveries owed
    /// somewhere, the run's next step. `false` for an end that changes only
    /// this runner's own phase.
    pub advance: bool,
}

impl OutcomeDecision {
    /// Nothing to journal — the report is history already written.
    pub(crate) fn none() -> Self {
        Self {
            events: Vec::new(),
            advance: false,
        }
    }

    /// Journal `events` and drain the boundary they create.
    pub(crate) fn advance(events: Vec<SessionEvent>) -> Self {
        Self {
            events,
            advance: true,
        }
    }

    /// Journal `events`; no boundary follows.
    pub(crate) fn record(events: Vec<SessionEvent>) -> Self {
        Self {
            events,
            advance: false,
        }
    }
}

/// Something the actor should do at a boundary.
#[derive(Debug, Clone)]
pub(crate) enum RunnerAction {
    /// Put a finished child runner's result in the queue of the agent owed it.
    /// One shape for a subagent's report and a nested run's output — which is
    /// why nesting needs no delivery machinery of its own.
    Deliver {
        to: AgentId,
        child: RunnerId,
        part: SubAgentResultPart,
    },
    /// Begin one execution of one workflow step.
    StartStep { run: RunnerId, start: StepStart },
    /// The run is over and succeeded, carrying the last step's output.
    FinishRun { run: RunnerId, output: Value },
    /// The run is over and failed.
    FailRun { run: RunnerId, error: String },
}

/// One execution of one workflow step. Carries everything needed to both
/// spawn the agent and journal the log entry.
#[derive(Debug, Clone)]
pub(crate) struct StepStart {
    pub index: u32,
    pub step: String,
    pub agent: AgentId,
    pub attempt: u32,
    /// The entry this came out of; `None` for the start step.
    pub from: Option<u32>,
    /// The transition condition that matched, if any.
    pub via: Option<String>,
    pub input: String,
}

/// Work a runner found undone at load — a dead process's leavings, described
/// so the actor can self-send the command that repairs it. A self-send rather
/// than direct work, because recovery must not persist: the repair arrives as
/// an ordinary command, down the same path a live one would take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Repair {
    /// The sandbox create was in flight or had failed retryably: re-attempt
    /// it. Safe precisely because no turn can have run under it.
    Provision,
    /// A subagent was mid-run. Its run is over; the parent is owed the failure
    /// like any other terminal result.
    FailInterruptedSub { id: RunnerId },
    /// A run's step was mid-flight. Suspend it — the step's effect on the
    /// shared workspace is unknown, so a person decides between retrying and
    /// abandoning.
    SuspendInterruptedRun { id: RunnerId },
    /// A run that has not begun. Let it start its first step.
    AdvanceRun { id: RunnerId },
    /// A fork whose seed never landed. Nothing else can finish one: seeding is
    /// session-owned work with no journal of its own.
    ReseedFork { id: RunnerId },
}
