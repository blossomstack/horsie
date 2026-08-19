//! The layers an agent's own tools are added through.
//!
//! Each one wraps the sandbox's toolbox and answers a handful of names itself
//! by `ask`ing the owning [`AgentActor`](crate::agent_loop::AgentActor), so the
//! state behind those tools stays durable — journaled and replayed like any
//! other agent state — instead of living in whatever process the runtime
//! happens to be. Everything they do not claim goes straight through, which is
//! what keeps an ordinary `bash` call as cheap as it was.
//!
//! Two of them rather than one because they are equipped independently: an
//! agent has timers always, and capabilities only when its runner gave it
//! some.

use crate::agent_loop::agent_actor::AgentCommand;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{ToolOutcome, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
pub(super) struct TimerToolbox {
    pub(super) inner: Arc<dyn Toolbox>,
    pub(super) actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::agent_loop::timers::timer_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
        use crate::agent_loop::timers::{CancelSelector, TimerId, TimerKind};
        use horsie_agentcore::ToolCallError;
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".to_string(),
                        ));
                    }
                };
                let Some(after_secs) = input
                    .get("after_secs")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".to_string(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let Some(message) = input
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.message must be a non-empty string".to_string(),
                    ));
                };
                let id = self
                    .actor
                    .ask(|reply| AgentCommand::ArmTimer {
                        label,
                        message,
                        kind,
                        after_secs,
                        reply,
                    })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(ToolOutcome::Result(serde_json::json!({ "timer_id": id.0 })))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::ListTimers { reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views)
                    .map(ToolOutcome::Result)
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".to_string(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::CancelTimer { selector, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(ToolOutcome::Result(serde_json::json!({ "cancelled": ids })))
            }
            _ => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

/// Wraps an agent's toolbox, adding every tool this agent's capabilities answer
/// for.
///
/// One layer for all of them rather than a decorator each: which capability
/// owns a name is decided by the offer scan on the mailbox, so a second place
/// deciding it here could only disagree. `names` is what the scan will accept,
/// captured when the run started — everything else goes straight to the sandbox
/// without a mailbox round trip, which is what keeps an ordinary `bash` call as
/// cheap as it was.
pub(super) struct CapabilityToolbox {
    pub(super) inner: Arc<dyn Toolbox>,
    pub(super) specs: Vec<horsie_agentcore::ToolSpec>,
    /// What this run found, sent on with every call it forwards. The specs above
    /// were built from it, so a capability refusing an argument on the mailbox
    /// refuses against the same list the model was shown.
    pub(super) facts: Arc<crate::sessions::runners::loading::AgentFacts>,
    pub(super) actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for CapabilityToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(self.specs.iter().cloned());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, horsie_agentcore::ToolCallError> {
        use horsie_agentcore::ToolCallError;
        if !self.specs.iter().any(|s| s.name == name) {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let call = crate::sessions::runners::message::ToolCall {
            id: tool_call_id.to_string(),
            name: name.to_string(),
            input,
        };
        let facts = Arc::clone(&self.facts);
        self.actor
            .ask(|reply| AgentCommand::CapabilityCall { call, facts, reply })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
    }
}
