//! The layers an agent's own tools are added through.
//!
//! A capability that has tools wraps the toolbox: it answers the names it
//! claims by sending the owning
//! [`AgentActor`](crate::agent_loop::AgentActor) one of its own commands, so
//! the state behind those tools stays durable — journaled and replayed like any
//! other agent state — instead of living in whatever process the runtime
//! happens to be. Everything a layer does not claim goes straight through to
//! the layer beneath it and ultimately to the sandbox, which is what keeps an
//! ordinary `bash` call as cheap as it was.
//!
//! **A name is resolved here and nowhere else.** The layer that claims one says
//! what it becomes, so what reaches the actor is a command that names its own
//! capability. Downstream of this file nothing matches a tool name at all.
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
use crate::agent_loop::capabilities::{Answering, CapCommand, Mailbox, ToolReply};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// The agent's own mailbox, in the shape a capability's layer can reach it.
///
/// Built once per run and shared by every layer. Handing this to
/// [`Capability::layer`](crate::agent_loop::capabilities::Capability::layer)
/// rather than the actor's address is what lets a capability compose its layer
/// without knowing the actor's command enum — and what lets a test compose one
/// with no actor at all.
pub(super) struct AgentMailbox {
    pub(super) actor: horsie_actor::ActorRef<AgentCommand>,
}

#[async_trait]
impl Mailbox for AgentMailbox {
    /// The command is built from the reply channel here, at the one place that
    /// has one — which is also the only place that knows the answer is an
    /// [`AgentCommand`] at all.
    async fn send(
        &self,
        make: Box<dyn FnOnce(ToolReply) -> CapCommand + Send>,
    ) -> Result<ToolOutcome, ToolCallError> {
        self.actor
            .ask(|reply| AgentCommand::Capability(make(reply)))
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
    }
}

/// One tool a capability claims: what the model is shown, and what a call to it
/// becomes.
///
/// The two are declared together because they are one decision. A name and its
/// command are the only place they ever meet: past here the name is gone, and a
/// capability is handed the arm it built rather than a string to match.
pub(crate) struct ClaimedTool {
    spec: ToolSpec,
    into_command: Arc<dyn Fn(Value, Answering) -> CapCommand + Send + Sync>,
}

impl ClaimedTool {
    /// Advertise `spec`, and turn a call to it into the command `into_command`
    /// builds.
    pub(crate) fn new(
        spec: ToolSpec,
        into_command: impl Fn(Value, Answering) -> CapCommand + Send + Sync + 'static,
    ) -> Self {
        Self {
            spec,
            into_command: Arc::new(into_command),
        }
    }
}

/// One capability's layer: the names it claims, and everything else passed
/// through.
///
/// The claims are what this capability advertised for this run, captured when
/// the run started. A call for one of those names goes to the mailbox as this
/// capability's own command, where its state is and where it can journal what
/// it did; a call for anything else goes straight to the layer beneath without
/// a mailbox round trip.
struct ClaimedTools {
    inner: Arc<dyn Toolbox>,
    claims: Vec<ClaimedTool>,
    mailbox: Arc<dyn Mailbox>,
}

impl ClaimedTools {
    fn claimed(&self, name: &str) -> Option<&ClaimedTool> {
        self.claims.iter().find(|t| t.spec.name == name)
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
        let mut specs: Vec<ToolSpec> = self.claims.iter().map(|t| t.spec.clone()).collect();
        specs.extend(
            self.inner
                .specs()
                .into_iter()
                .filter(|s| self.claimed(&s.name).is_none()),
        );
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        let Some(tool) = self.claimed(name) else {
            return self.inner.execute(name, input, tool_call_id).await;
        };
        let into_command = Arc::clone(&tool.into_command);
        let call = tool_call_id.to_string();
        self.mailbox
            .send(Box::new(move |reply| {
                into_command(input, Answering { call, reply })
            }))
            .await
    }
}

/// Wrap `inner` in a layer that answers `claims` on the agent's mailbox.
///
/// `inner` untouched when there is nothing to claim, so a capability whose
/// advertisement is conditional — a muted `ask_user`, a `spawn_agent` past its
/// depth — adds no layer at all rather than one that only forwards.
#[must_use]
pub(crate) fn claiming(
    inner: Arc<dyn Toolbox>,
    claims: Vec<ClaimedTool>,
    mailbox: &Arc<dyn Mailbox>,
) -> Arc<dyn Toolbox> {
    if claims.is_empty() {
        return inner;
    }
    Arc::new(ClaimedTools {
        inner,
        claims,
        mailbox: Arc::clone(mailbox),
    })
}
