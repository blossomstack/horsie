//! Placeholder: filled in by its own task.

use super::capabilities::Capability;
use super::message::ChildOutcome;
use super::{Action, AgentId, AgentLifecycle, Emit, Runner, RunnerEvent, SessionView, TurnEnd};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {}

impl Runner for State {
    fn actions(&self, _view: &SessionView) -> Vec<Action> {
        Vec::new()
    }

    fn outcome(&self) -> Option<ChildOutcome> {
        None
    }

    fn busy(&self) -> bool {
        false
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn capabilities_mut(&mut self) -> &mut [Capability] {
        &mut self.capabilities
    }

    fn apply(&mut self, _event: &RunnerEvent) {}
}

impl AgentLifecycle for State {
    fn on_agent_started(&self, _agent: AgentId) -> Emit {
        (Vec::new(), Vec::new())
    }

    fn on_agent_ended(&self, _agent: AgentId, _end: &TurnEnd) -> Emit {
        (Vec::new(), Vec::new())
    }

    fn on_agent_halted(&self, _agent: AgentId, _reason: &str) -> Emit {
        (Vec::new(), Vec::new())
    }
}
