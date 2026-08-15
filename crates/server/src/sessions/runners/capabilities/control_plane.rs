//! Placeholder: filled in by its own task.

use super::{CapEvent, Decision, Handler};
use crate::sessions::runners::action::AgentSpec;
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlPlaneCapability;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {}

impl Handler for ControlPlaneCapability {
    fn setup(&self, _spec: &mut AgentSpec) {}

    fn handle(&self, _caller: Caller, _msg: &Message) -> Option<Decision> {
        None
    }

    fn apply(&mut self, _event: &CapEvent) {}
}
