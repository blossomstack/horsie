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

pub mod action;
pub mod capabilities;
pub mod conversation;
pub mod ids;
pub mod message;
pub mod runtime;
pub mod state;
pub mod subagent;
pub mod workflow;

pub use ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
pub use state::{RunnerRecord, SessionState};

use action::Action;
use capabilities::Capability;
use message::ChildOutcome;
use serde::{Deserialize, Serialize};

/// What a runner decided: events for its own slice, actions for the session.
/// The same pair a capability returns, one level up.
pub type Emit = (Vec<RunnerEvent>, Vec<Action>);

/// How a turn ended, narrowed to the ways that mean something to a runner.
///
/// [`crate::agent_loop::AgentOutcome`] minus the reports that are not endings
/// at all — usage to bank, and a turn *beginning*. Those mean the same thing
/// to every runner, so the session answers them itself rather than routing
/// them, which is what keeps this enum small enough that each runner can match
/// it exhaustively instead of carrying arms for cases it can never see.
#[derive(Debug, Clone)]
pub enum TurnEnd {
    /// The agent produced its output — structured, or its final text.
    Concluded { output: serde_json::Value },
    /// The agent parked on one or more questions. Carries none of them: they
    /// belong to the agent that asked and are answered through it.
    Asked,
    /// `terminal` means the sandbox is gone and no later message brings it
    /// back; anything else is a turn that can be retried.
    Failed { error: String, terminal: bool },
    /// The agent parked awaiting its timers.
    Parked,
    /// The process died inside the turn, and the agent said so at recovery.
    Interrupted,
}

/// What a runner may know about the session around it.
///
/// Its own slice plus this, and nothing else. Handing a runner the whole
/// [`SessionState`] would let a workflow read a conversation's turn status,
/// which is the coupling this split exists to remove — so everything
/// cross-runner arrives as a call instead, and everything session-wide arrives
/// here as a number.
#[derive(Debug, Clone, Copy)]
pub struct SessionView {
    /// Nothing starts before the sandbox exists. One gate, checked once, for
    /// every runner.
    pub runtime_ready: bool,
    /// How deep this runner sits, for the one budget nesting needs.
    pub depth: u32,
    /// How many agents this session already has running, against the
    /// session-wide cap. A property of the sandbox, so it is the session's
    /// number rather than a per-runner one.
    pub active_agents: u32,
}

/// One unit of work, and the agents that carry it out.
///
/// Implemented by the *state* struct rather than by a separate unit type: the
/// state is all a runner has, so a second object would only be a place for a
/// field to hide that does not survive a reload.
pub trait Runner {
    /// What I want started, given the state as it now is.
    ///
    /// Pure and idempotent, called at every boundary — which is what lets
    /// creation and recovery take the same path. A `run()` that fired once
    /// would need a second entry for recovery, and the suppression that
    /// implies is where the bugs live.
    fn actions(&self, view: &SessionView) -> Vec<Action>;

    /// My ending, translated into the vocabulary of whoever created me.
    ///
    /// `None` while I am still going, and *always* `None` for a conversation:
    /// a conversation owes nobody a result, root or not, which is what lets
    /// `parent` mean provenance rather than debt.
    fn outcome(&self) -> Option<ChildOutcome> {
        None
    }

    /// Whether I have work in flight, so the session must not unload.
    fn busy(&self) -> bool;

    /// What my agents are equipped with.
    fn capabilities(&self) -> &[Capability];

    /// The same, for folding a capability's own event into it. Separate from
    /// [`Runner::capabilities`] so the read-only path stays read-only.
    fn capabilities_mut(&mut self) -> &mut [Capability];

    /// Fold one of my own events. Pure — no clock, no ids, no randomness.
    fn apply(&mut self, event: &RunnerEvent);
}

/// What a runner must answer for the agents it starts.
///
/// A separate trait, and [`RunnerState::lifecycle`] returns `None` for the one
/// runner that owns no agents — so "a runner with no agents cannot be handed
/// an agent event" is a fact about the type rather than an unreachable arm
/// somebody has to keep true.
///
/// No method switches on *which* agent, because one runner owns exactly one
/// agent role: a workflow owns several step agents over time, but they are all
/// step agents. A runner's state, its agent role and its outcome vocabulary
/// are one triple.
pub trait AgentLifecycle {
    fn on_agent_started(&self, agent: AgentId) -> Emit;

    /// The method that separates the runners: the same ending is a result for
    /// one, an input to a graph for another.
    fn on_agent_ended(&self, agent: AgentId, end: &TurnEnd) -> Emit;

    fn on_agent_halted(&self, agent: AgentId, reason: &str) -> Emit;
}

/// An `AgentSettings` with nothing set.
///
/// Test-only, and deliberately not a `Default` on `AgentSettings` itself: a
/// settings with an empty `model` names no provider, so production code must
/// not be able to build one by accident. The two runners that need a `Default`
/// state need it only so `RunnerState::empty_for` can build one, which is
/// itself test scaffolding.
#[cfg(test)]
#[must_use]
pub(crate) fn empty_settings() -> crate::sessions::spec::AgentSettings {
    crate::sessions::spec::AgentSettings {
        model: String::new(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: 0,
        mcp_servers: Vec::new(),
        memory_spaces: Vec::new(),
        thinking_effort: None,
        max_concurrent_subagents: None,
        instructions: None,
        plugins: Vec::new(),
        auto_compact: None,
        control_plane: None,
    }
}

/// A runner's own slice.
///
/// One arm per kind, and the session never matches on it to make a decision:
/// it hands the whole value back to the impl that owns it. A closed enum
/// rather than an opaque blob so the journal stays typed and a shape change
/// fails to compile where it should.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerState {
    Conversation(conversation::State),
    SubAgent(subagent::State),
    Workflow(Box<workflow::State>),
    Runtime(runtime::State),
}

/// One runner's event, tagged with the runner that owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerEvent {
    Conversation(conversation::Event),
    SubAgent(subagent::Event),
    Workflow(workflow::Event),
    Runtime(runtime::Event),
    /// A capability's own event, folded into whichever capability owns that
    /// arm. Here rather than on each runner because every runner holds
    /// capabilities and none of them needs to know which.
    Capability(capabilities::CapEvent),
}

macro_rules! dispatch {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            RunnerState::Conversation(s) => s.$method($($arg),*),
            RunnerState::SubAgent(s) => s.$method($($arg),*),
            RunnerState::Workflow(s) => s.$method($($arg),*),
            RunnerState::Runtime(s) => s.$method($($arg),*),
        }
    };
}

impl Runner for RunnerState {
    fn actions(&self, view: &SessionView) -> Vec<Action> {
        // One gate in front of everything: nothing starts before the sandbox
        // the work would run on exists.
        if !view.runtime_ready && !matches!(self, RunnerState::Runtime(_)) {
            return Vec::new();
        }
        dispatch!(self, actions, view)
    }

    fn outcome(&self) -> Option<ChildOutcome> {
        dispatch!(self, outcome)
    }

    fn busy(&self) -> bool {
        dispatch!(self, busy)
    }

    fn capabilities(&self) -> &[Capability] {
        dispatch!(self, capabilities)
    }

    fn capabilities_mut(&mut self) -> &mut [Capability] {
        dispatch!(self, capabilities_mut)
    }

    fn apply(&mut self, event: &RunnerEvent) {
        // A capability's event is folded into the capability that owns it,
        // whichever runner is holding it.
        if let RunnerEvent::Capability(e) = event {
            for cap in dispatch!(self, capabilities_mut) {
                capabilities::Handler::apply(cap, e);
            }
            return;
        }
        dispatch!(self, apply, event);
    }
}

impl RunnerState {
    /// An empty slice of the right shape. Tests only — live code builds a
    /// runner's state from the args it was created with.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn empty_for(kind: RunnerKind) -> Self {
        match kind {
            RunnerKind::Conversation => Self::Conversation(conversation::State::default()),
            RunnerKind::SubAgent => Self::SubAgent(subagent::State::default()),
            RunnerKind::Workflow => Self::Workflow(Box::default()),
            RunnerKind::Runtime => Self::Runtime(runtime::State::default()),
        }
    }

    /// What this runner must answer for its agents, or `None` when it owns
    /// none. The runtime is the only `None`.
    #[must_use]
    pub fn lifecycle(&self) -> Option<&dyn AgentLifecycle> {
        match self {
            Self::Conversation(s) => Some(s),
            Self::SubAgent(s) => Some(s),
            Self::Workflow(s) => Some(s.as_ref()),
            Self::Runtime(_) => None,
        }
    }
}
