//! `spawn_agent` and `subagent_status`: handing work to a worker, and being
//! told how it went.
//!
//! Held by every agent that may delegate — a conversation, a workflow step, a
//! subagent, which is what makes a subagent of a subagent ordinary rather than
//! a case. One implementation serves all of them because of invariant 6: an
//! agent may not conclude while it has outstanding children, so every parent
//! does the identical thing on a report — put it in its own queue.
//!
//! # What the move changed
//!
//! Two things, and both are simplifications.
//!
//! The session-side twin mapped each outstanding child to *which of its agents*
//! asked, because one runner held one capability for many agents. Here the
//! capability belongs to the agent that asked, and a report ([`Reported`]) goes
//! into *this agent's* queue — so there is no address to keep, and what is
//! outstanding is a set of children rather than a map.
//!
//! Creating the child is still the session's: it owns the tree. So a spawn is
//! [`Spawned::Ask`] — the request goes to the session and the model's call is
//! parked on it, because the session's answer is what that call's result is
//! made of. The child's id is minted *here*, so the event the actor journals
//! and the request it sends name the same child, and a replay lands the id the
//! log already holds.
//!
//! # What this file decides, and what the actor does with it
//!
//! Every function here returns a narrow value — words for the model, a request
//! for the session, a child's report — and never an event. Writing to the log
//! is the actor's, because it is the only place that knows when a write is
//! durable: a capability that could journal would be able to report success for
//! work a crash loses.
//!
//! # Where the gates went, and why
//!
//! The old session actor made three refusals. They do not all belong in one
//! place, because they are not all facts one actor can see:
//!
//! - **Depth stays here.** How deep this agent sits is fixed when it is
//!   equipped and never changes, so the capability holds it
//!   ([`SubAgentCapability::depth`]) and refuses in flight — one message, no
//!   round trip, and the model is told before a child is journaled.
//! - **The concurrency cap moved to the session.** It is a count over the whole
//!   tree, and only the session holds the tree. An agent-side check would be
//!   counting something it cannot see. So the capability asks, the session
//!   refuses, and [`SessionReply::Refused`] comes back as the tool's result.
//!   That also ends a mismatch the old shape had: the *number* came from the
//!   caller's preset while the *count* was already session-wide.
//! - **`"caller is not a known agent"` is gone.** The session resolves the
//!   agent that spoke before a call becomes a command at all, so by the time
//!   this runs the caller has already been attributed. The refusal belongs at
//!   that lookup, which is the only place that can still fail.
//!
//! A refusal made here is [`Spawned::Told`] and journals nothing — the model is
//! told no, and no trace of a child that never existed reaches the log. It is
//! still *answered*: the layer that advertised `spawn_agent` is what turns a
//! call into this capability's command, so every call to that name arrives
//! here, including the ones over budget. If this file did not answer them the
//! name would have to be left to the layers beneath, the last of which is the
//! open-namespace sandbox that answers to every name — so the model would be
//! answered by the sandbox and never learn it had hit a budget.

use super::{Mailbox, SessionReply, SessionRequest};
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::agent_loop::{AgentCatalog, AgentCommand, Incoming};
use crate::sessions::runners::action::RunnerArgs;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::AgentFacts;
use crate::sessions::runners::message::{ChildMsg, ChildOutcome, SubAgentOutcome};
use crate::sessions::spec::AgentSettings;
use crate::sessions::subagents::MAX_SUBAGENT_DEPTH;
use horsie_agentcore::{ToolSpec, Toolbox};
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The tool that delegates.
pub const SPAWN_TOOL: &str = "spawn_agent";
/// The tool that reads back what is still running.
pub const STATUS_TOOL: &str = "subagent_status";

/// One agent's delegated work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentCapability {
    /// Fixed when this agent was equipped: what children inherit. A child's
    /// equipment is decided at the moment its parent was equipped, not at the
    /// moment it is spawned, so a settings change mid-session cannot give two
    /// siblings different tools.
    pub child_settings: AgentSettings,
    /// How deep the agent holding this already sits. Also fixed at equip time,
    /// which is what lets the depth gate be answered without asking anyone.
    pub depth: u32,
}

/// The workers this agent has in flight.
///
/// Fields private to this file, so "is a report still owed?" is a question this
/// capability answers rather than a set anything can read and reinterpret.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubAgentState {
    /// Spawns asked for and not yet answered, by the model's call.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent [`SubAgentCapability::reloaded`] asks about again — and the
    /// session dedupes on the worker's agent id rather than starting a second
    /// child.
    #[serde(default)]
    requested: BTreeMap<String, Pending>,
    /// Children that exist and still owe a report.
    ///
    /// A set rather than the session-side map to an `AgentId`: this belongs to
    /// the agent that asked, so the report goes into its own queue and there is
    /// no address to keep. It is also the single fact behind both questions
    /// anyone asks about delegated work — is a report still owed, and may this
    /// agent finish (invariant 6).
    #[serde(default)]
    outstanding: BTreeSet<RunnerId>,
}

impl SubAgentState {
    /// Whether this agent still owes somebody a report it cannot produce yet.
    ///
    /// Invariant 6, named: an agent may not conclude while it has outstanding
    /// children. Cheap because the state is folded from the same journal as
    /// everything else the actor reads at a turn boundary.
    #[must_use]
    pub(crate) fn holds_conclusion(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// A spawn was asked of the session.
    pub(crate) fn requested(&mut self, call: String, pending: Pending) {
        self.requested.insert(call, pending);
    }

    /// The session created it, so a report is now owed.
    pub(crate) fn started(&mut self, call: &str) {
        if let Some(pending) = self.requested.remove(call) {
            self.outstanding.insert(pending.child);
        }
    }

    /// The session would not create it.
    pub(crate) fn dropped(&mut self, call: &str) {
        self.requested.remove(call);
    }

    /// This child's report reached the queue.
    pub(crate) fn reported(&mut self, child: RunnerId) {
        self.outstanding.remove(&child);
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take it by reference. An accessor kept for a
/// caller that does not exist is how a private field stops being private.
impl SubAgentState {
    /// The spawns the session has not answered yet.
    #[must_use]
    pub(crate) fn pending(&self) -> &BTreeMap<String, Pending> {
        &self.requested
    }

    /// The children that still owe a report.
    #[must_use]
    pub(crate) fn outstanding(&self) -> &BTreeSet<RunnerId> {
        &self.outstanding
    }
}

/// One spawn asked of the session and not yet answered.
///
/// The whole request rather than its ids alone, because a re-ask on load has to
/// send the *same* request again: a `Requested` that recorded only the child
/// would come back after a crash knowing a worker was wanted and unable to say
/// what for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The runner the worker will be.
    pub child: RunnerId,
    /// The worker's own agent, and the session's dedupe key: a replayed request
    /// carries the same id, so the session recognises a child it has already
    /// created instead of starting a second one.
    pub agent: AgentId,
    pub label: String,
    pub task: String,
    pub agent_type: Option<String>,
}

/// The tool's arguments. Deserialised here so the schema and this type are one
/// declaration rather than two that can drift.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub label: String,
    pub task: String,
    /// A plugin-declared agent type, or `None` for a worker that inherits its
    /// parent's instructions and tools.
    pub agent_type: Option<String>,
}

/// What a call to `spawn_agent` came to.
#[derive(Debug)]
pub(crate) enum Spawned {
    /// Told why, in words, and the run carries on. Journals nothing.
    Told(String),
    /// Journal the request, put it to the session, and park the call on it:
    /// the session's answer is what this call's result is made of.
    Ask { pending: Pending, note: String },
}

impl SubAgentCapability {
    #[must_use]
    pub fn new(child_settings: AgentSettings, depth: u32) -> Self {
        Self {
            child_settings,
            depth,
        }
    }

    /// The model called `spawn_agent`.
    ///
    /// Every call to that name lands here, so every answer is this
    /// capability's: a malformed call and a call over budget are both answered
    /// in words rather than passed on — see the module doc on why.
    pub(crate) fn spawned(&self, input: &Value, catalog: Option<&AgentCatalog>) -> Spawned {
        let req: Request = match serde_json::from_value(input.clone()) {
            Ok(req) => req,
            Err(e) => {
                return Spawned::Told(format!(
                    "`{SPAWN_TOOL}` was called with arguments it cannot read: {e}"
                ));
            }
        };
        // `depth` is the *asking* agent's, so the first worker of a
        // conversation is spawned from depth 0 and lands at 1 — which is why
        // the bound is `>=` and not `>`. The concurrency cap is not checked
        // here: see the module doc.
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Spawned::Told(format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached"));
        }
        // Refused before anything is journaled, and refused *here*, because this
        // is the layer that advertised the list: an error naming what exists is
        // only possible where the list is. The child would refuse it too — its
        // runtime capability resolves the type against the library it loads —
        // but by then a child exists, and its complaint reaches the model as a
        // failed worker rather than as an answer to the call that named it.
        let agent_type = match resolve_type(req.agent_type.as_deref(), catalog) {
            Ok(resolved) => resolved,
            Err(reason) => return Spawned::Told(reason),
        };
        // Both ids are minted here rather than in `apply`: a decision may be
        // non-deterministic, a fold may not. Replay must land the ids the log
        // recorded, so the event and the request name the same child and
        // neither invents one.
        //
        // The worker's agent id is minted *with* its runner id, and not when
        // the worker's agent starts, because `spawn_agent`'s result names it.
        // Two ids and not one: a runner and an agent are separate spaces, and a
        // workflow runner owns many agents, so an equality would hold here and
        // be false there.
        let pending = Pending {
            child: RunnerId::new_v4(),
            agent: AgentId::new_v4(),
            label: req.label,
            task: req.task,
            agent_type,
        };
        let note = format!("spawning subagent {}", pending.label);
        Spawned::Ask { pending, note }
    }

    /// The request a [`Pending`] names.
    ///
    /// One function and two callers — the spawn, and the re-ask on load —
    /// because the second has to send exactly what the first sent. Built from
    /// the journaled request plus this capability's own config, so nothing in
    /// it is minted twice.
    pub(crate) fn request(&self, call: &str, pending: &Pending) -> SessionRequest {
        SessionRequest::StartRunner {
            call: call.to_string(),
            id: pending.child,
            kind: RunnerKind::SubAgent,
            args: Box::new(RunnerArgs::SubAgent {
                agent: pending.agent,
                label: pending.label.clone(),
                task: pending.task.clone(),
                agent_type: pending.agent_type.clone(),
                settings: Box::new(self.child_settings.clone()),
            }),
        }
    }

    /// Everything asked for and never answered, asked again. Empty when there
    /// is nothing outstanding.
    ///
    /// A `Requested` still in the fold is a request the dead process may never
    /// have sent: the journal write comes first, so the window between it and
    /// the session hearing anything is exactly what this closes. Re-asked with
    /// the ids already recorded — the same call, the same child, the same agent
    /// — which is what lets the session tell a repeat from a new spawn.
    ///
    /// Requests and nothing else: the
    /// [`SubAgentRequested`](crate::agent_loop::AgentDomainEvent::SubAgentRequested)
    /// this reads is still the only fact, and a second copy of it would say a
    /// second spawn was wanted.
    pub(crate) fn reloaded(&self, state: &SubAgentState) -> Vec<SessionRequest> {
        state
            .requested
            .iter()
            .map(|(call, pending)| self.request(call, pending))
            .collect()
    }
}

/// What the session said about a spawn this agent asked for.
#[derive(Debug)]
pub(crate) enum Child {
    Started { call: String, child: RunnerId },
    Dropped { call: String, reason: String },
}

impl Child {
    /// The parked call this answers.
    pub(crate) fn call(&self) -> &str {
        match self {
            Self::Started { call, .. } | Self::Dropped { call, .. } => call,
        }
    }

    /// The parked call's result. A refusal the model cannot see is a tool call
    /// that never returns, so it comes back as that call's result with
    /// `is_error` set rather than as a fresh answer.
    pub(crate) fn result(&self) -> ToolResultInput {
        let (output, is_error) = match self {
            Self::Started { child, .. } => (format!("Subagent spawned: {child}"), false),
            Self::Dropped { reason, .. } => (reason.clone(), true),
        };
        ToolResultInput {
            tool_call_id: self.call().to_string(),
            output,
            is_error,
        }
    }
}

/// The session answered a spawn this capability asked for.
///
/// `None` when this reply answers something that is not a spawn of ours, so a
/// reply meant for another capability is not claimed by whichever looked first.
pub(crate) fn replied(state: &SubAgentState, reply: &SessionReply) -> Option<Child> {
    let child = state.requested.get(reply.call())?.child;
    Some(match reply {
        SessionReply::Done { call } => Child::Started {
            call: call.clone(),
            child,
        },
        SessionReply::Refused { call, reason } => Child::Dropped {
            call: call.clone(),
            reason: reason.clone(),
        },
    })
}

/// A child reported, and this is what its report becomes in the queue.
#[derive(Debug)]
pub struct Reported {
    pub child: RunnerId,
    pub item: Incoming,
}

/// A child moved.
///
/// `None` for anything this capability is not owed a report by.
///
/// **No production sender reaches this yet.** The runners redesign routes a
/// child's movement through `sessions::runners::message`, which nothing
/// forwards to an agent so far, so only tests call this. Kept and kept public
/// rather than deleted: the behaviour is the settled answer for when that
/// forwarding lands, and deleting it would have to be re-derived.
pub fn child(state: &SubAgentState, m: &ChildMsg) -> Option<Reported> {
    match m {
        ChildMsg::Outcome {
            child,
            outcome: ChildOutcome::SubAgent(o),
        } => {
            // Not one of mine: fall through as `None` rather than deliver
            // somebody else's report, so "addressed by owner" is enforced by
            // the same return type as "nothing to report".
            state.outstanding.contains(child).then(|| {
                deliver(
                    *child,
                    match o {
                        SubAgentOutcome::Completed { label, report } => SubAgentResultPart {
                            subagent_id: child.to_string(),
                            label: label.clone(),
                            status: "completed".to_string(),
                            text: report.clone(),
                            spawned_at_ms: 0,
                            ended_at_ms: 0,
                        },
                        SubAgentOutcome::Failed { label, error } => {
                            failed_part(*child, label.clone(), error.clone())
                        }
                    },
                )
            })
        }
        // A run's outcome is the workflow capability's even when both are held
        // by the same agent. The outcome's kind and the owning capability have
        // to agree, and `None` is how they do.
        ChildMsg::Outcome {
            outcome: ChildOutcome::Workflow(_),
            ..
        } => None,
        // A child that died still owes its asker an answer: the agent is
        // sitting on a spawn it was told succeeded.
        ChildMsg::Failed { child, error } => state.outstanding.contains(child).then(|| {
            deliver(
                *child,
                failed_part(*child, child.to_string(), error.clone()),
            )
        }),
        // A worker is runnable the moment it is created; only a fork has a seed
        // that can land later.
        ChildMsg::Ready { .. } => None,
    }
}

/// The report, in the shape this agent's own queue takes.
///
/// The actor journals the acknowledgement and the queueing together, so they
/// cannot land apart: a crash before the write replays as a report still
/// outstanding, and it is delivered again.
fn deliver(child: RunnerId, part: SubAgentResultPart) -> Reported {
    Reported {
        child,
        item: Incoming::SubAgent {
            id: child.to_string(),
            part: Box::new(part),
        },
    }
}

/// What `subagent_status` shows the model.
///
/// Only what is still running: a child that reported has been delivered into
/// this agent's own transcript, so listing it again would show the model its
/// own history back.
pub(crate) fn render_status(state: &SubAgentState) -> String {
    if state.outstanding.is_empty() {
        return "No subagents are running.".to_string();
    }
    let mut text = format!("{} subagent(s) running:", state.outstanding.len());
    for child in &state.outstanding {
        text.push_str(&format!("\n- {child}"));
    }
    text
}

/// Why this turn's end is not the agent finishing, when a report is still owed.
///
/// Invariant 6, asked directly rather than merged out of a broadcast: a step
/// whose subagent still owes it a report must not conclude, and must not be
/// nudged either, because a nudge is for a turn that ended with *nothing*
/// coming. The child's report is work this agent has not seen yet, and
/// concluding on it is how a superseded step lands a second conclusion on an
/// index the run already routed past.
pub(crate) fn holds(state: &SubAgentState) -> Option<String> {
    state
        .holds_conclusion()
        .then(|| format!("{} subagent(s) still owe a report", state.outstanding.len()))
}

/// The plugin-declared agents this load found, if it found a library at all.
///
/// Read off the facts rather than held on the capability: the catalogue is what
/// the *current* library declares, and a capability is folded from a journal
/// that may be older than the plugins installed since.
fn catalog(facts: &AgentFacts) -> Option<&AgentCatalog> {
    facts.shared.as_deref().map(|shared| shared.agents.as_ref())
}

/// The same catalogue, in the form a command can carry: shared rather than
/// borrowed, because the call it is refused against happens on the mailbox long
/// after the facts it was advertised from went out of scope.
fn shared_catalog(facts: &AgentFacts) -> Option<Arc<AgentCatalog>> {
    facts
        .shared
        .as_ref()
        .map(|shared| Arc::clone(&shared.agents))
}

/// A requested agent type, checked against the catalogue that was advertised.
///
/// `None` for an omitted or blank type, which is the general-purpose worker and
/// not a mistake. The error is the model's to read, so it names what does exist.
fn resolve_type(
    requested: Option<&str>,
    catalog: Option<&AgentCatalog>,
) -> Result<Option<String>, String> {
    let Some(requested) = requested.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if catalog.is_some_and(|c| c.get(requested).is_some()) {
        return Ok(Some(requested.to_string()));
    }
    let known = catalog.map(AgentCatalog::names).unwrap_or_default();
    Err(if known.is_empty() {
        format!("no agent type '{requested}': this session has no agent types installed")
    } else {
        format!(
            "no agent type '{requested}'; installed types are {}",
            known.join(", ")
        )
    })
}

/// A dead worker's report, in the shape the parent's inbox takes.
///
/// The timestamps are zero because this capability holds neither: they live on
/// the child's `RunnerRecord`. A client shows no duration rather than one that
/// never happened.
fn failed_part(child: RunnerId, label: String, error: String) -> SubAgentResultPart {
    SubAgentResultPart {
        subagent_id: child.to_string(),
        label,
        status: "failed".to_string(),
        text: error,
        spawned_at_ms: 0,
        ended_at_ms: 0,
    }
}

impl SubAgentCapability {
    /// Both tools, claimed by this capability's own layer rather than pushed as
    /// a layer in `setup`.
    ///
    /// That is the change the move made: a layer pushed there runs on the
    /// agent's task, where there is no mailbox to journal an intent on and no
    /// way to park the call while the session answers. A name claimed here
    /// becomes an [`AgentCommand`] on the actor's mailbox, which can do both.
    ///
    /// A budget the model can only ever be refused by advertises nothing. A
    /// tool like that is worse than no tool: it spends prompt on a capability
    /// that does not exist and invites a retry loop against a fixed number.
    ///
    /// The facts are what carry the agent catalogue, and this is the only reason
    /// [`super::Capability::layer`] takes them: the types a session can spawn are found
    /// by the workspace scan, which runs in a capability that sorts *after* this
    /// one — so the layers are composed on the run's task, after `provide`, and
    /// this list is built there. A model not shown it can only guess at a name,
    /// and every guess is refused.
    fn claims(&self, facts: &AgentFacts) -> Vec<ClaimedTool> {
        if self.child_settings.max_subagents() == 0 || self.depth >= MAX_SUBAGENT_DEPTH {
            return Vec::new();
        }
        let mut description = "Spawn a subagent to work on a task independently and in parallel. \
            Returns immediately with the subagent's id; its result or failure is \
            automatically delivered back to you as a message. Continue with independent \
            work, or wait if none remains; do not poll subagent_status or call it \
            repeatedly. Spawning fails when the session's subagent limits (depth or \
            concurrency) are reached."
            .to_string();
        let mut properties = serde_json::Map::new();
        properties.insert(
            "label".to_string(),
            json!({
                "type": "string",
                "description": "A short human-readable label for the subagent (a few words)."
            }),
        );
        properties.insert(
            "task".to_string(),
            json!({
                "type": "string",
                "description": "The complete, self-contained task for the subagent. It \
                    inherits your model and tools but not your conversation — include \
                    everything it needs to know."
            }),
        );
        // The catalogue goes in the description, not in a JSON `enum`: a bare
        // list of names says nothing about when to pick one, and `description`
        // is the whole point of the frontmatter field. With no agents installed
        // the parameter is absent entirely, so a session with no plugins sees
        // exactly the tool it saw before they existed.
        let catalog = catalog(facts).filter(|c| !c.is_empty());
        if let Some(catalog) = catalog {
            let listing = catalog
                .iter()
                .map(|a| format!("- {}: {}", a.def.name, a.def.description))
                .collect::<Vec<_>>()
                .join("\n");
            description.push_str(&format!(
                "\n\nInstalled agent types, each with its own instructions, tools and \
                 expertise. Pass one as `agent_type` when its description fits the task \
                 better than a general-purpose subagent would:\n{listing}"
            ));
            properties.insert(
                "agent_type".to_string(),
                json!({
                    "type": "string",
                    "description": "Name of an installed agent type, from the list above. \
                        Omit for a general-purpose subagent that inherits your own \
                        instructions and tools."
                }),
            );
        }
        // Captured here, at the one moment the scan exists, and carried on the
        // command: the mailbox has no scan of its own, and a refusal has to
        // name the same list the model was shown.
        let advertised = shared_catalog(facts);
        vec![
            ClaimedTool::new(
                ToolSpec {
                    name: SPAWN_TOOL.to_string(),
                    description,
                    input_schema: json!({
                        "type": "object",
                        "required": ["label", "task"],
                        "properties": properties,
                    }),
                },
                move |input, to| AgentCommand::SubAgentSpawn {
                    input,
                    catalog: advertised.clone(),
                    answering: to,
                },
            ),
            ClaimedTool::new(
                ToolSpec {
                    name: STATUS_TOOL.to_string(),
                    description: "Inspect subagent status only for a user-requested progress \
                        update or to diagnose a suspected result-delivery problem. Do not poll or \
                        call this tool repeatedly: terminal results and failures are automatically \
                        delivered to you as messages. Lists the subagents you spawned that are \
                        still running."
                        .to_string(),
                    input_schema: json!({ "type": "object", "properties": {} }),
                },
                |_input, to| AgentCommand::SubAgentStatus { answering: to },
            ),
        ]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl SubAgentCapability {
    pub fn name(&self) -> &'static str {
        "sub_agent"
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(facts), mailbox)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, facts, settings, specs_of};
    use super::*;
    use crate::agent_loop::AgentDomainEvent;
    use crate::agent_loop::capabilities::{Capabilities, Capability};
    use crate::sessions::runners::message::WorkflowOutcome;

    /// An agent that may delegate, sitting at the top of the tree.
    fn cap() -> SubAgentCapability {
        at_depth(0)
    }

    /// The same, `depth` levels down.
    fn at_depth(depth: u32) -> SubAgentCapability {
        SubAgentCapability::new(settings(), depth)
    }

    /// The capability itself, for the tests that only ask what it advertises.
    fn delegating() -> Capability {
        Capability::SubAgent(cap())
    }

    /// Journal one event, the way the actor journals it.
    ///
    /// Through `apply` rather than by reaching into the fields, so a test's
    /// setup is the same fold a replay does — a capability decides from state
    /// it does not own, and this is how that state comes to be.
    fn fold(sub_agent: SubAgentState, event: AgentDomainEvent) -> SubAgentState {
        crate::agent_loop::AgentState {
            sub_agent,
            ..crate::agent_loop::AgentState::default()
        }
        .apply(event)
        .sub_agent
    }

    /// The state a run of events leaves behind.
    fn folded(events: impl IntoIterator<Item = AgentDomainEvent>) -> SubAgentState {
        let mut state = SubAgentState::default();
        for event in events {
            state = fold(state, event);
        }
        state
    }

    /// The plain spawn every test that is not about arguments asks for.
    fn spawn_input() -> Value {
        json!({"label": "l", "task": "t"})
    }

    /// A spawn asked for under call `t1`, with the event the actor journals
    /// for it.
    fn asked(c: &SubAgentCapability) -> (Pending, AgentDomainEvent) {
        let Spawned::Ask { pending, .. } = c.spawned(&spawn_input(), None) else {
            panic!("expected the session to be asked for a child");
        };
        let event = AgentDomainEvent::SubAgentRequested {
            call: "t1".to_string(),
            pending: pending.clone(),
        };
        (pending, event)
    }

    /// Ask for a worker and let the session say yes, which is the only way
    /// there is to an outstanding child.
    fn running(c: &SubAgentCapability) -> (SubAgentState, RunnerId) {
        let (pending, requested) = asked(c);
        let state = folded([
            requested,
            AgentDomainEvent::SubAgentStarted {
                call: "t1".to_string(),
            },
        ]);
        (state, pending.child)
    }

    /// What the model was told, having checked that it was told rather than
    /// obeyed. A refusal is not a fact about the agent, and `Told` is the arm
    /// the actor journals nothing and sends nothing for.
    fn refusal(spawned: Spawned) -> String {
        match spawned {
            Spawned::Told(text) => text,
            Spawned::Ask { pending, .. } => {
                panic!("a refused spawn must not reach the session: {pending:?}")
            }
        }
    }

    /// The facts a load leaves behind when the shared library declared these
    /// agents — built the way the runtime's scan builds them, so what the
    /// capability reads here is the shape it reads in a session.
    fn facts_with(agents: &[(&str, &str)]) -> AgentFacts {
        let catalog: AgentCatalog = agents
            .iter()
            .map(|(name, description)| crate::agent_loop::CatalogAgent {
                plugin: "fd".into(),
                def: horsie_support::plugin::agents::PluginAgentDef {
                    name: (*name).to_string(),
                    description: (*description).to_string(),
                    model: None,
                    tools: Vec::new(),
                    prompt: "be one".into(),
                },
            })
            .collect();
        AgentFacts {
            shared: Some(std::sync::Arc::new(crate::agent_loop::SharedContext {
                agents: std::sync::Arc::new(catalog),
                ..crate::agent_loop::SharedContext::default()
            })),
            ..AgentFacts::default()
        }
    }

    /// What the model is shown for `spawn_agent` under these facts.
    fn spawn_spec(facts: &AgentFacts) -> ToolSpec {
        specs_of(&delegating(), facts)
            .into_iter()
            .find(|t| t.name == SPAWN_TOOL)
            .expect("spawn_agent is advertised")
    }

    /// The event and the request must name the same child. If they ever differ,
    /// the log records a child nothing created and the agent waits for ever.
    ///
    /// One `Pending` is what makes them agree: the actor journals it and builds
    /// the request from it, so there is no second place for an id to be minted.
    #[test]
    fn a_spawn_journals_and_asks_for_the_same_child() {
        let c = cap();
        let Spawned::Ask { pending, note } =
            c.spawned(&json!({"label": "read the flake", "task": "look"}), None)
        else {
            panic!("expected the session to be asked for a child");
        };
        assert!(
            note.contains("read the flake"),
            "the park says nothing about what is being waited on: {note}"
        );

        let request = c.request("t1", &pending);
        let SessionRequest::StartRunner {
            call,
            id,
            kind,
            args,
        } = &request
        else {
            panic!("expected a runner to be started, got {request:?}");
        };
        assert_eq!(
            *id, pending.child,
            "the log records a child nothing was asked for"
        );
        assert_eq!(call, "t1", "the call the session answers is the parked one");
        assert_eq!(*kind, RunnerKind::SubAgent);
        let RunnerArgs::SubAgent { agent, label, .. } = args.as_ref() else {
            panic!("expected subagent args, got {args:?}");
        };
        // The worker's agent is decided here, with its runner, because
        // `spawn_agent`'s result names it — and it is its *own* id, not the
        // runner's. Two spaces on purpose: a workflow runner owns many agents,
        // so an equality that held for a worker would be false for a run.
        assert_ne!(agent.as_uuid(), pending.child.as_uuid());
        assert_eq!(
            *agent, pending.agent,
            "the agent asked for is not the one journaled, so a replay would \
             mint a second worker the session cannot recognise"
        );
        assert_eq!(label, "read the flake");

        // Nothing is outstanding yet: the session has not said it exists.
        let state = folded([AgentDomainEvent::SubAgentRequested {
            call: "t1".to_string(),
            pending: pending.clone(),
        }]);
        assert!(state.outstanding().is_empty());
        assert_eq!(state.pending().get("t1"), Some(&pending));
    }

    /// The bound on nesting, and the one gate that stays agent-side: how deep
    /// this agent sits is fixed when it is equipped, so no round trip is needed
    /// to answer it. Without it a worker that spawns a worker is a machine that
    /// runs until something else stops it.
    #[test]
    fn a_spawn_at_the_depth_limit_is_refused_without_asking_the_session() {
        // The last depth that may still delegate, and the first that may not.
        let ok = at_depth(MAX_SUBAGENT_DEPTH - 1).spawned(&spawn_input(), None);
        assert!(matches!(ok, Spawned::Ask { .. }));

        // `refusal` is what asserts the session is never asked: a `Told` is the
        // whole answer, so there is no request to send and nothing to journal.
        let told = at_depth(MAX_SUBAGENT_DEPTH).spawned(&spawn_input(), None);
        assert_eq!(refusal(told), "max subagent depth 4 reached");
    }

    /// **The concurrency cap is the session's now**, because it is a count over
    /// the whole tree and only the session holds the tree. So the capability
    /// asks, and the refusal comes back as a reply — which must still reach the
    /// model, against the call it parked, or the spawn never returns.
    #[test]
    fn a_cap_refusal_from_the_session_becomes_the_parked_calls_result() {
        let c = cap();
        let (_, requested) = asked(&c);
        let state = folded([requested.clone()]);

        let dropped = replied(
            &state,
            &SessionReply::Refused {
                call: "t1".into(),
                reason: "8 subagents already active".into(),
            },
        )
        .expect("the reply answers a call I made");
        assert!(
            matches!(dropped, Child::Dropped { .. }),
            "a refusal the model cannot see is a call that never returns: {dropped:?}"
        );
        assert_eq!(dropped.call(), "t1");
        let result = dropped.result();
        assert_eq!(result.tool_call_id, "t1");
        assert_eq!(result.output, "8 subagents already active");
        assert!(result.is_error);

        // And the intent is retracted, so nothing re-asks for it on load and no
        // report is ever expected from a child that does not exist.
        let state = folded([
            requested,
            AgentDomainEvent::SubAgentDropped {
                call: "t1".to_string(),
            },
        ]);
        assert!(state.pending().is_empty());
        assert!(state.outstanding().is_empty());
    }

    /// The session said yes: the parked call gets the child's id, and only now
    /// is a report owed.
    #[test]
    fn a_started_child_answers_the_parked_call_and_becomes_outstanding() {
        let c = cap();
        let (pending, requested) = asked(&c);
        let state = folded([requested.clone()]);

        let started = replied(&state, &SessionReply::Done { call: "t1".into() }).expect("mine");
        let result = started.result();
        assert_eq!(result.tool_call_id, "t1");
        assert!(!result.is_error);
        assert!(
            result.output.starts_with("Subagent spawned: "),
            "the model is owed the id it can ask about: {}",
            result.output
        );

        let state = folded([
            requested,
            AgentDomainEvent::SubAgentStarted {
                call: "t1".to_string(),
            },
        ]);
        let outstanding: Vec<RunnerId> = state.outstanding().iter().copied().collect();
        assert_eq!(outstanding, vec![pending.child]);
        assert!(state.pending().is_empty(), "an answered request is over");
        assert!(
            result
                .output
                .contains(&outstanding.first().expect("one outstanding").to_string()),
            "the id the model was told is not the child that is outstanding"
        );
    }

    /// **The crash window, and the only test that closes it.**
    ///
    /// A journal that stops between `Requested` and the session's answer is a
    /// spawn the session may never have heard of — the write comes first, so
    /// the window is real — and the model is parked on a call that will never
    /// be answered. The load has to ask again.
    ///
    /// Everything asserted here is an *identity*: the same call, the same
    /// child, the same worker. That is the whole mechanism — it is what makes
    /// the second ask recognisable as a repeat rather than a second spawn.
    ///
    /// A re-ask journals nothing, and now cannot: it is a list of requests and
    /// no events at all. The `Requested` it reads is still the only record, and
    /// a second copy would say a second spawn was wanted.
    #[test]
    fn a_spawn_the_session_never_answered_is_asked_again_on_load() {
        let c = cap();
        let (pending, requested) = asked(&c);
        let first = c.request("t1", &pending);
        let SessionRequest::StartRunner { call, id, args, .. } = &first else {
            panic!("expected a runner to be started, got {first:?}");
        };
        let (first_call, child) = (call.clone(), *id);
        let RunnerArgs::SubAgent { agent, .. } = args.as_ref() else {
            panic!("expected subagent args, got {args:?}");
        };
        let worker = *agent;

        // The cut. Nothing is folded past the request, and what comes back is
        // read off the journal the way a new process reads it — the capability
        // included, because it is what the re-ask is built from.
        let state = crate::agent_loop::AgentState {
            capabilities: Capabilities::new(vec![Capability::SubAgent(c)]),
            ..crate::agent_loop::AgentState::default()
        }
        .apply(requested);
        let written = serde_json::to_string(&state).expect("write");
        let reloaded: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        let [Capability::SubAgent(back)] = reloaded.capabilities.iter().collect::<Vec<_>>()[..]
        else {
            panic!("the journal changed which capability this is");
        };

        let again = back.reloaded(&reloaded.sub_agent);
        let [
            SessionRequest::StartRunner {
                call,
                id,
                kind,
                args,
            },
        ] = again.as_slice()
        else {
            panic!("expected exactly one re-ask, got {again:?}");
        };
        assert_eq!(
            *call, first_call,
            "a re-ask under a different call answers a park nobody is holding"
        );
        assert_eq!(
            *id, child,
            "the re-ask names a child the log never recorded"
        );
        assert_eq!(*kind, RunnerKind::SubAgent);
        let RunnerArgs::SubAgent {
            agent, label, task, ..
        } = args.as_ref()
        else {
            panic!("expected subagent args, got {args:?}");
        };
        assert_eq!(
            *agent, worker,
            "a re-ask that mints a fresh worker id is a second child the \
             session has no way to recognise"
        );
        // And the request itself survived, or the session would be asked for a
        // worker with nothing to do.
        assert_eq!(label, "l");
        assert_eq!(task, "t");
    }

    /// The other half: a request the session already answered must not be asked
    /// for again, or every load spawns the children of the last one.
    #[test]
    fn a_spawn_the_session_answered_is_not_asked_again() {
        let c = cap();
        let (state, _) = running(&c);
        assert!(
            c.reloaded(&state).is_empty(),
            "the session already created this child; asking again duplicates it"
        );

        // A refusal retracts the intent too, so a load after one asks for
        // nothing rather than re-running into the same budget.
        let (_, requested) = asked(&c);
        let state = folded([
            requested,
            AgentDomainEvent::SubAgentDropped {
                call: "t1".to_string(),
            },
        ]);
        assert!(c.reloaded(&state).is_empty());
    }

    /// A reply for a call this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_call_i_never_made_is_not_mine() {
        let c = cap();
        let (_, requested) = asked(&c);
        let state = folded([requested]);
        assert!(
            replied(
                &state,
                &SessionReply::Done {
                    call: "someone-else".into()
                }
            )
            .is_none()
        );
    }

    /// Arguments this capability cannot read are still *its* call to answer:
    /// the layer that advertised the name is what routed the call here, and
    /// leaving it unanswered would hand the model to the sandbox instead.
    #[test]
    fn a_malformed_spawn_is_refused_in_words_and_journals_nothing() {
        let told = cap().spawned(&json!({"label": "l"}), None);
        assert!(refusal(told).contains(SPAWN_TOOL));
    }

    /// A report goes into this agent's own queue, which is what replaced the
    /// session-side address: the capability belongs to the agent that asked.
    #[test]
    fn a_completed_report_is_queued_for_the_agent_that_asked() {
        let c = cap();
        let (state, kid) = running(&c);

        let reported = child(
            &state,
            &ChildMsg::Outcome {
                child: kid,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "found it".into(),
                }),
            },
        )
        .expect("mine");
        assert_eq!(reported.child, kid);
        let Incoming::SubAgent { id, part } = &reported.item else {
            panic!("expected a queued report, got {:?}", reported.item);
        };
        assert_eq!(*id, kid.to_string());
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "found it");
        assert_eq!(part.subagent_id, kid.to_string());
        assert_eq!(part.label, "l");

        let state = fold(
            state,
            AgentDomainEvent::SubAgentReported {
                child: reported.child,
            },
        );
        assert!(
            state.outstanding().is_empty(),
            "a reported child owes nothing"
        );
    }

    /// A failure is a report too. An agent blocked on a worker that died and
    /// was never told would wait for ever.
    #[test]
    fn a_failed_outcome_is_queued_as_a_failed_part() {
        let c = cap();
        let (state, kid) = running(&c);
        let reported = child(
            &state,
            &ChildMsg::Outcome {
                child: kid,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Failed {
                    label: "l".into(),
                    error: "it broke".into(),
                }),
            },
        )
        .expect("mine");
        let Incoming::SubAgent { part, .. } = &reported.item else {
            panic!("expected a queued report, got {:?}", reported.item);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "it broke");
    }

    /// A child that died before it said anything takes the same path: the asker
    /// is holding an id it was told was real.
    #[test]
    fn a_child_that_never_ran_is_reported_as_failed() {
        let c = cap();
        let (state, kid) = running(&c);
        let reported = child(
            &state,
            &ChildMsg::Failed {
                child: kid,
                error: "the create failed".into(),
            },
        )
        .expect("mine");
        assert_eq!(reported.child, kid);
        let Incoming::SubAgent { part, .. } = &reported.item else {
            panic!("expected a queued report, got {:?}", reported.item);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "the create failed");
    }

    /// A child this capability did not create is not its business. Without the
    /// gate, a sibling's report would be queued for an agent that never asked.
    #[test]
    fn an_outcome_for_a_child_i_did_not_create_is_not_mine() {
        let c = cap();
        let (state, _) = running(&c);
        assert!(
            child(
                &state,
                &ChildMsg::Outcome {
                    child: RunnerId::new_v4(),
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "r".into(),
                    }),
                }
            )
            .is_none()
        );
    }

    /// A run's outcome is the workflow capability's, even for a child id this
    /// one holds. Two capabilities must never both plausibly claim an outcome.
    #[test]
    fn a_workflow_outcome_is_never_mine() {
        let c = cap();
        let (state, kid) = running(&c);
        assert!(
            child(
                &state,
                &ChildMsg::Outcome {
                    child: kid,
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                        output: json!("done")
                    }),
                }
            )
            .is_none()
        );
        // And a worker is runnable the moment it exists, so `Ready` is a fork's
        // message and not this one's.
        assert!(child(&state, &ChildMsg::Ready { child: kid }).is_none());
    }

    /// **Invariant 6.** A turn ending while a report is still owed does not
    /// finish this agent: the child's report is work it has not seen yet, and
    /// concluding here is how a superseded step lands a second conclusion on an
    /// index the run already routed past.
    ///
    /// Asked directly now, rather than merged out of a broadcast: the actor
    /// puts the question to this capability and reads the note back, so a hold
    /// can no longer be invisible to it.
    #[test]
    fn a_turn_ending_with_an_outstanding_child_holds_the_conclusion() {
        let c = cap();
        let (state, kid) = running(&c);
        assert!(state.holds_conclusion());
        assert_eq!(
            holds(&state).as_deref(),
            Some("1 subagent(s) still owe a report"),
            "a turn ending with a report still owed must not let the agent finish"
        );

        // The report lands, and the very next boundary lets it go.
        let reported = child(
            &state,
            &ChildMsg::Outcome {
                child: kid,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "done".into(),
                }),
            },
        )
        .expect("mine");
        let state = fold(
            state,
            AgentDomainEvent::SubAgentReported {
                child: reported.child,
            },
        );
        assert!(!state.holds_conclusion());
        assert!(
            holds(&state).is_none(),
            "an agent owed nothing has no opinion about its turn ending"
        );
    }

    /// A child merely *asked for* holds nothing: the session has not said it
    /// exists, and the spawn call is parked, so the agent is not finishing
    /// anyway. Reading `requested` here would hold a conclusion for a child
    /// that was refused.
    #[test]
    fn a_requested_child_does_not_hold_the_conclusion() {
        let c = cap();
        let (_, requested) = asked(&c);
        let state = folded([requested]);
        assert!(!state.pending().is_empty());
        assert!(!state.holds_conclusion());
        assert!(holds(&state).is_none());
    }

    /// A status read journals nothing — it is a string, so it cannot — and it
    /// lists only what is still going: a child that reported is already in this
    /// agent's own transcript.
    #[test]
    fn status_lists_the_outstanding_children_and_journals_nothing() {
        let c = cap();
        let (state, kid) = running(&c);
        assert!(render_status(&state).contains(&kid.to_string()));

        let state = fold(state, AgentDomainEvent::SubAgentReported { child: kid });
        assert!(!render_status(&state).contains(&kid.to_string()));
    }

    /// Both tools, claimed by this capability's own layer — which is what
    /// routes the call to the mailbox, where the intent can be journaled and the
    /// call parked.
    #[test]
    fn it_advertises_both_tools() {
        assert_eq!(
            advertised_by(&delegating(), &facts()),
            vec![SPAWN_TOOL, STATUS_TOOL]
        );
    }

    /// **The catalogue is the whole reason `tools` takes facts.**
    ///
    /// The types a session can spawn are found by the workspace scan, which runs
    /// in a capability equipped *after* this one — `sub_agent` sorts early so it
    /// wins the `spawn_agent` name against the open-namespace sandbox. So the
    /// list cannot be known when this capability is built, and is read at
    /// advertisement time instead. Without it the model is told a parameter
    /// exists and never told what may go in it, which is a guess with a refusal
    /// waiting behind it.
    ///
    /// In the description rather than a JSON `enum`, because a bare list of
    /// names says nothing about when to pick one.
    #[test]
    fn the_scans_agent_types_are_advertised_with_their_descriptions() {
        let facts = facts_with(&[("code-reviewer", "reviews diffs for real bugs")]);
        let spawn = spawn_spec(&facts);
        assert!(
            spawn
                .description
                .contains("- code-reviewer: reviews diffs for real bugs"),
            "the model is not told which agent types exist: {}",
            spawn.description
        );
        assert!(
            spawn.input_schema["properties"]["agent_type"].is_object(),
            "a listed type the model cannot pass is no offer at all"
        );
    }

    /// And a scan that found none leaves no trace: a session with no plugins
    /// sees exactly the tool it saw before agent types existed — no vestigial
    /// parameter, no empty list to reason about.
    #[test]
    fn with_no_agent_types_found_the_parameter_is_absent() {
        for facts in [facts(), facts_with(&[])] {
            let spawn = spawn_spec(&facts);
            assert!(spawn.input_schema["properties"]["agent_type"].is_null());
            assert!(!spawn.description.contains("agent_type"));
        }
    }

    /// **Refused where the list is.** An error naming what exists is only
    /// possible where the catalogue is, and it is the same one the description
    /// was built from — the facts ride in on the call. The child would refuse
    /// this too, when it failed to load as a type nothing declares, but by then
    /// a worker has been journaled and the model reads a dead subagent instead
    /// of an answer.
    #[test]
    fn an_unknown_agent_type_is_refused_and_names_the_installed_ones() {
        let installed = facts_with(&[("code-reviewer", "reviews"), ("scout", "searches")]);
        let told = cap().spawned(
            &json!({"label": "l", "task": "t", "agent_type": "reviewer"}),
            catalog(&installed),
        );
        assert_eq!(
            refusal(told),
            "no agent type 'reviewer'; installed types are code-reviewer, scout"
        );

        // With nothing installed the refusal says so, rather than naming an
        // empty list.
        let none = facts();
        let told = cap().spawned(
            &json!({"label": "l", "task": "t", "agent_type": "reviewer"}),
            catalog(&none),
        );
        assert_eq!(
            refusal(told),
            "no agent type 'reviewer': this session has no agent types installed"
        );
    }

    /// A type that exists reaches the worker, and an omitted or blank one is the
    /// general-purpose worker rather than a mistake.
    #[test]
    fn an_installed_type_is_carried_to_the_child_and_a_blank_one_is_not() {
        let facts = facts_with(&[("code-reviewer", "reviews")]);
        for (input, expected) in [
            (
                json!({"label": "l", "task": "t", "agent_type": "code-reviewer"}),
                Some("code-reviewer"),
            ),
            (json!({"label": "l", "task": "t", "agent_type": "  "}), None),
            (json!({"label": "l", "task": "t"}), None),
        ] {
            let Spawned::Ask { pending, .. } = cap().spawned(&input, catalog(&facts)) else {
                panic!("expected the session to be asked for a child");
            };
            assert_eq!(pending.agent_type.as_deref(), expected);
        }
    }

    /// A budget the model can only ever be refused by advertises no tool at
    /// all, so it never meets one that cannot work.
    #[test]
    fn a_spent_budget_advertises_nothing() {
        let mut zero = settings();
        zero.max_concurrent_subagents = Some(0);
        assert!(
            advertised_by(
                &Capability::SubAgent(SubAgentCapability::new(zero, 0)),
                &facts()
            )
            .is_empty()
        );
        assert!(
            advertised_by(
                &Capability::SubAgent(SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH)),
                &facts()
            )
            .is_empty()
        );
    }

    /// `outstanding` is what invariant 6 and every delivery are decided from,
    /// so losing it in the journal loses the agent: the report arrives on a
    /// process that has since rehydrated the session.
    #[test]
    fn the_outstanding_children_survive_the_journal_round_trip() {
        let c = cap();
        let (sub_agent, kid) = running(&c);
        let state = crate::agent_loop::AgentState {
            capabilities: Capabilities::new(vec![Capability::SubAgent(c)]),
            sub_agent,
            ..crate::agent_loop::AgentState::default()
        };

        let written = serde_json::to_string(&state).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            back.sub_agent
                .outstanding()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![kid],
            "a reload that lost the outstanding child would let the agent conclude \
             with a report still coming"
        );
        let [Capability::SubAgent(back)] = back.capabilities.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.depth, 0, "the gate this agent carries is config too");
    }
}
