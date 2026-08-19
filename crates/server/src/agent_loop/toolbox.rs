//! The agent's own tools, composed once over the sandbox.
//!
//! A capability that has tools claims them here: it answers the names it claims
//! by sending the owning [`AgentActor`](crate::agent_loop::AgentActor) one of
//! its own commands, so the state behind those tools stays durable — journaled
//! and replayed like any other agent state — instead of living in whatever
//! process the runtime happens to be. Everything unclaimed goes straight to the
//! sandbox, which is what keeps an ordinary `bash` call as cheap as it was.
//!
//! **A name is resolved here and nowhere else.** The claim says which of the
//! actor's own command arms a call becomes, so what reaches the actor already
//! names the capability that answers it. Downstream of this file nothing
//! matches a tool name at all.
//!
//! # One composition, and why that is the simplification
//!
//! Every capability used to wrap the toolbox in a layer of its own, so an
//! ordinary `bash` call fell through up to thirteen nested decorators, each
//! scanning its own claims, before it reached the sandbox. Which capability won
//! a contested name was decided by where it sat in the enabled list — a silent
//! precedence win, discovered only by reading the list.
//!
//! [`compose`] collects every claim from the enabled list into one table and
//! wraps the sandbox once. Two capabilities claiming one name is now a
//! [`ClaimConflict`] rather than a winner, and there is no capability-versus-
//! capability ordering left to get wrong. What remains is a single fallthrough:
//! the agent's own names, then the sandbox's open namespace.

use crate::agent_loop::agent_actor::AgentCommand;
use crate::agent_loop::capabilities::{Answering, Capability};
use crate::sessions::runners::loading::AgentFacts;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// One tool a capability claims: what the model is shown, and what a call to it
/// becomes.
///
/// The two are declared together because they are one decision, which is what
/// rules out a name that was claimed and cannot be mapped. A name and its
/// command are the only place they ever meet: past here the name is gone, and a
/// capability is handed the arm it built rather than a string to match.
pub(crate) struct ClaimedTool {
    spec: ToolSpec,
    into_command: Arc<dyn Fn(Value, Answering) -> AgentCommand + Send + Sync>,
}

/// The command is a closure, so only the spec can be shown — which is the part
/// a failed composition needs to name anyway.
impl std::fmt::Debug for ClaimedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedTool")
            .field("name", &self.spec.name)
            .finish()
    }
}

impl ClaimedTool {
    /// Advertise `spec`, and turn a call to it into the command `into_command`
    /// builds.
    pub(crate) fn new(
        spec: ToolSpec,
        into_command: impl Fn(Value, Answering) -> AgentCommand + Send + Sync + 'static,
    ) -> Self {
        Self {
            spec,
            into_command: Arc::new(into_command),
        }
    }

    /// What the model is shown for this tool.
    pub(crate) fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// The command a call to this tool becomes.
    pub(crate) fn command(&self, input: Value, answering: Answering) -> AgentCommand {
        (self.into_command)(input, answering)
    }
}

/// Two capabilities claimed one tool name.
///
/// A bug in [`assemble`](crate::sessions::runners::assemble) or in whoever
/// equipped an agent afterwards, never anything a person or a model can cause:
/// the enabled list is first-party code, and the claims are fixed by the time
/// composition runs. It is still an error rather than a panic because
/// composition happens on the run's own spawned task, where a panic is caught
/// by the runtime, nobody joins the handle, and the turn simply never ends —
/// the failure mode this reports as a plain run failure instead.
#[derive(Debug)]
pub(crate) struct ClaimConflict {
    pub name: String,
    /// The capability that claimed the name first.
    pub held_by: &'static str,
    /// The one that claimed it again.
    pub claimed_by: &'static str,
}

impl std::fmt::Display for ClaimConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} and {} both claim the tool `{}`",
            self.held_by, self.claimed_by, self.name
        )
    }
}

/// Every tool the enabled capabilities claim, in the enabled list's own order.
///
/// The order is the model's: it is what [`advertise`] shows, and changing it
/// changes what every model sees. It is *not* precedence — a name belongs to
/// exactly one capability or composition fails, so there is nothing left for an
/// order to resolve.
///
/// [`AgentFacts`] because an advertisement can depend on what the load found:
/// `sub_agent` lists the installed agent types, and only the workspace scan
/// knows them. That is why composition happens on the run's own task after
/// `provide` rather than when the agent is equipped.
pub(crate) fn claims(
    caps: &[Capability],
    facts: &AgentFacts,
) -> Result<Vec<ClaimedTool>, ClaimConflict> {
    let mut claimed: Vec<ClaimedTool> = Vec::new();
    let mut owner: HashMap<String, &'static str> = HashMap::new();
    for cap in caps {
        for tool in cap.claims(facts) {
            match owner.insert(tool.spec().name.clone(), cap.name()) {
                Some(held_by) => {
                    return Err(ClaimConflict {
                        name: tool.spec().name.clone(),
                        held_by,
                        claimed_by: cap.name(),
                    });
                }
                None => claimed.push(tool),
            }
        }
    }
    Ok(claimed)
}

/// What the model is shown: the agent's own tools first, then everything the
/// sandbox has that nothing claimed.
///
/// The filter is the whole of the fallthrough rule. A capability that claims a
/// sandbox name is advertised once — by the capability, because that is who
/// will answer a call to it.
#[must_use]
pub(crate) fn advertise(claims: &[ClaimedTool], sandbox: &dyn Toolbox) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = claims.iter().map(|t| t.spec().clone()).collect();
    specs.extend(
        sandbox
            .specs()
            .into_iter()
            .filter(|s| !claims.iter().any(|t| t.spec().name == s.name)),
    );
    specs
}

/// The whole toolbox an equipped agent runs with: its capabilities' claims over
/// the sandbox.
///
/// The claims are what this agent advertised for this run, captured when the run
/// started. A call for one of those names goes to the actor as that capability's
/// own command, where its state is and where it can journal what it did; a call
/// for anything else goes straight to the sandbox without a mailbox round trip.
struct AgentTools {
    /// Advertisement order, which is the enabled list's order.
    claims: Vec<ClaimedTool>,
    /// One lookup per call, rather than a scan per capability. Indexes into
    /// `claims` so the order above stays the one the model was shown.
    by_name: HashMap<String, usize>,
    sandbox: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for AgentTools {
    fn specs(&self) -> Vec<ToolSpec> {
        advertise(&self.claims, self.sandbox.as_ref())
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        let Some(tool) = self.by_name.get(name).and_then(|i| self.claims.get(*i)) else {
            return self.sandbox.execute(name, input, tool_call_id).await;
        };
        let call = tool_call_id.to_string();
        // The command is built from the reply channel here, at the one place
        // that has one — `ask`'s shape. The channel is never a capability's:
        // the actor decides *when* the answer goes out, which is after the
        // events behind it are durable.
        self.actor
            .ask(move |reply| tool.command(input, Answering { call, reply }))
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
    }
}

/// Build the toolbox this run hands the model.
///
/// `sandbox` is what `provide` composed — the runtime, MCP, memory — and it is
/// returned untouched when nothing is claimed, so a prompt-only agent adds no
/// indirection at all.
///
/// # Errors
/// [`ClaimConflict`] when two capabilities claim one tool name.
pub(crate) fn compose(
    caps: &[Capability],
    facts: &AgentFacts,
    sandbox: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
) -> Result<Arc<dyn Toolbox>, ClaimConflict> {
    let claims = claims(caps, facts)?;
    if claims.is_empty() {
        return Ok(sandbox);
    }
    let by_name = claims
        .iter()
        .enumerate()
        .map(|(i, t)| (t.spec().name.clone(), i))
        .collect();
    Ok(Arc::new(AgentTools {
        claims,
        by_name,
        sandbox,
        actor,
    }))
}
