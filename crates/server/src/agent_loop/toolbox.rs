//! The layers an agent's own tools are added through.
//!
//! A capability that has tools wraps the toolbox: it answers the names it
//! claims by `ask`ing the owning
//! [`AgentActor`](crate::agent_loop::AgentActor), so the state behind those
//! tools stays durable — journaled and replayed like any other agent state —
//! instead of living in whatever process the runtime happens to be. Everything
//! a layer does not claim goes straight through to the layer beneath it and
//! ultimately to the sandbox, which is what keeps an ordinary `bash` call as
//! cheap as it was.
//!
//! # One layer each, and why that is the simplification
//!
//! There used to be a single layer holding every capability's specs at once,
//! because there was a single question — which capability owns a name — and the
//! offer scan on the mailbox answered it. That worked, and it meant a capability
//! list had to satisfy two orderings that read opposite ways: first in offer
//! order, outermost in the toolbox.
//!
//! Now each capability wraps for itself and wrapping order *is* precedence, so
//! there is one rule instead of two. What a name resolves to is decided in the
//! same place for advertisement and for execution: the outermost layer that
//! claims it.

use crate::agent_loop::agent_actor::AgentCommand;
use async_trait::async_trait;
use horsie_agentcore::{ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// The agent's own mailbox, in the shape a toolbox layer can call.
///
/// Built once per run and shared by every layer, because what a capability
/// needs from the actor is exactly what a toolbox offers: a name, an input and
/// a call id in, an outcome out. Handing this to
/// [`Capability::layer`](crate::agent_loop::capabilities::Capability::layer)
/// rather than the actor's address is what lets a capability compose its layer
/// without knowing the actor's command enum — and what lets a test compose one
/// with no actor at all.
///
/// It advertises nothing itself. A layer that claims a name is what makes the
/// model able to reach this.
pub(super) struct AgentMailbox {
    /// What this run found, sent on with every call. The specs the layers
    /// advertise were built from it, so a capability refusing an argument on
    /// the mailbox refuses against the same list the model was shown.
    pub(super) facts: Arc<crate::sessions::runners::loading::AgentFacts>,
    pub(super) actor: horsie_actor::ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for AgentMailbox {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, horsie_agentcore::ToolCallError> {
        use horsie_agentcore::ToolCallError;
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

/// One capability's layer: the names it claims, and everything else passed
/// through.
///
/// `specs` is what this capability advertised for this run, captured when the
/// run started. A call for one of those names goes to the mailbox, where the
/// capability's state is and where it can journal what it did; a call for
/// anything else goes straight to the layer beneath without a mailbox round
/// trip.
struct ClaimedTools {
    inner: Arc<dyn Toolbox>,
    specs: Vec<ToolSpec>,
    mailbox: Arc<dyn Toolbox>,
}

impl ClaimedTools {
    fn claims(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }
}

#[async_trait]
impl Toolbox for ClaimedTools {
    /// Mine first, then everything beneath that I do not claim.
    ///
    /// Both halves are the precedence rule: the model is shown the outermost
    /// claimant's spec, and a name claimed twice is advertised once — by
    /// whichever layer will actually answer it.
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.specs.clone();
        specs.extend(
            self.inner
                .specs()
                .into_iter()
                .filter(|s| !self.claims(&s.name)),
        );
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, horsie_agentcore::ToolCallError> {
        match self.claims(name) {
            true => self.mailbox.execute(name, input, tool_call_id).await,
            false => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

/// Wrap `inner` in a layer that answers `specs` on the agent's mailbox.
///
/// `inner` untouched when there is nothing to claim, so a capability whose
/// advertisement is conditional — a muted `ask_user`, a `spawn_agent` past its
/// depth — adds no layer at all rather than one that only forwards.
#[must_use]
pub(crate) fn claiming(
    inner: Arc<dyn Toolbox>,
    specs: Vec<ToolSpec>,
    mailbox: &Arc<dyn Toolbox>,
) -> Arc<dyn Toolbox> {
    if specs.is_empty() {
        return inner;
    }
    Arc::new(ClaimedTools {
        inner,
        specs,
        mailbox: Arc::clone(mailbox),
    })
}
