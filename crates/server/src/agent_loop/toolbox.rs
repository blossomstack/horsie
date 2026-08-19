//! The layer an agent's own tools are added through.
//!
//! It wraps the sandbox's toolbox and answers the names this agent's
//! capabilities claim by `ask`ing the owning
//! [`AgentActor`](crate::agent_loop::AgentActor), so the state behind those
//! tools stays durable — journaled and replayed like any other agent state —
//! instead of living in whatever process the runtime happens to be. Everything
//! it does not claim goes straight through, which is what keeps an ordinary
//! `bash` call as cheap as it was.
//!
//! One layer, because there is one question: which capability owns a name, and
//! that is answered by the offer scan on the mailbox. There used to be three,
//! and the other two were tools the agent always had — a task list and its
//! timers — hand-wired beside a mechanism that already generalises them.

use crate::agent_loop::agent_actor::AgentCommand;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{ToolOutcome, Toolbox};
use serde_json::Value;
use std::sync::Arc;

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
