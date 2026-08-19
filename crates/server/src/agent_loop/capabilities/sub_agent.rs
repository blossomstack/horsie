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
//! capability belongs to the agent that asked, and [`Act::Enqueue`] puts a
//! report in *this agent's* queue — so there is no address to keep, and
//! [`SubAgentCapability::outstanding`] is a set.
//!
//! Creating the child is still the session's: it owns the tree. So a spawn is
//! [`Act::Ask`] plus [`Act::Park`] — the model's call is left dangling while
//! the session answers, and the reply supplies its result. The child's id is
//! minted *here*, so the event this journals and the request it sends name the
//! same child, and a replay lands the id the log already holds.
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
//!   agent that spoke before any capability is offered its call, so by the time
//!   this runs the caller has already been attributed. The refusal belongs at
//!   that lookup, which is the only place that can still fail.
//!
//! A refusal made here is a [`Decision::reply`] and journals nothing — the
//! model is told no, and no trace of a child that never existed reaches the
//! log. It is still *claimed*: declining would hand the call to the next
//! capability, and the last of those is the open-namespace sandbox, which
//! answers to every name — so the model would be answered by the sandbox and
//! never learn it had hit a budget.

use super::{
    Act, CapCommand, CapEvent, CapSlice, Capability, Decision, Mailbox, Msg, SessionReply,
    SessionRequest, TurnEvent,
};
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::agent_loop::{AgentCatalog, Incoming};
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
    /// Spawns asked for and not yet answered, by the model's call.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent [`Msg::Loaded`] asks about again — and the session dedupes on
    /// the worker's agent id rather than starting a second child.
    pub requested: BTreeMap<String, Pending>,
    /// Children that exist and still owe a report.
    ///
    /// A set rather than the session-side map to an `AgentId`: the capability
    /// belongs to the agent that asked, so the report goes into its own queue
    /// and there is no address to keep. It is also the single fact behind both
    /// questions anyone asks about delegated work — is a report still owed, and
    /// may this agent finish (invariant 6).
    pub outstanding: BTreeSet<RunnerId>,
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

/// What this capability records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A spawn was asked of the session, and this is what was asked for.
    Requested { call: String, pending: Pending },
    /// The session created it, so a report is now owed.
    Started { call: String },
    /// The session would not create it. Journaled because the
    /// [`Event::Requested`] before it was — this retracts a fact, and is not
    /// itself the refusal.
    Dropped { call: String },
    /// This child's report reached the queue.
    Reported { child: RunnerId },
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

/// What the model asked this capability to do.
pub enum Command {
    /// `spawn_agent`, with the catalogue the advertisement was built from.
    ///
    /// The catalogue rides on the command rather than being held, for the same
    /// reason it was never held before: it is what the *current* library
    /// declares, and this capability is folded from a journal that may be older
    /// than the plugins installed since. It is captured when the layer is
    /// composed, so a refusal names exactly the list the model was shown.
    Spawn {
        input: Value,
        catalog: Option<Arc<AgentCatalog>>,
    },
    /// `subagent_status`. No input: reading back what is running takes no
    /// arguments.
    Status,
}

impl SubAgentCapability {
    #[must_use]
    pub fn new(child_settings: AgentSettings, depth: u32) -> Self {
        Self {
            child_settings,
            depth,
            requested: BTreeMap::new(),
            outstanding: BTreeSet::new(),
        }
    }

    /// Whether this agent still owes somebody a report it cannot produce yet.
    ///
    /// Invariant 6, named: an agent may not conclude while it has outstanding
    /// children. Cheap because the capability holding `outstanding` is the same
    /// one offered [`TurnEvent::Ended`], in the same actor, folded from the
    /// same journal — no coordination at all.
    #[must_use]
    pub fn holds_conclusion(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// The model called `spawn_agent`.
    fn spawn(&self, call: &str, input: &Value, catalog: Option<&AgentCatalog>) -> Decision {
        let req: Request = match serde_json::from_value(input.clone()) {
            Ok(req) => req,
            // A capability that owns a tool name owns every call to it,
            // including the malformed ones — see the module doc on why this is
            // answered rather than declined.
            Err(e) => {
                return Decision::reply(
                    call,
                    format!("`{SPAWN_TOOL}` was called with arguments it cannot read: {e}"),
                );
            }
        };
        // `depth` is the *asking* agent's, so the first worker of a
        // conversation is spawned from depth 0 and lands at 1 — which is why
        // the bound is `>=` and not `>`. The concurrency cap is not checked
        // here: see the module doc.
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Decision::reply(
                call,
                format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached"),
            );
        }
        // Refused before anything is journaled, and refused *here*, because this
        // is the layer that advertised the list: an error naming what exists is
        // only possible where the list is. The child would refuse it too — its
        // runtime capability resolves the type against the library it loads —
        // but by then a child exists, and its complaint reaches the model as a
        // failed worker rather than as an answer to the call that named it.
        let agent_type = match resolve_type(req.agent_type.as_deref(), catalog) {
            Ok(resolved) => resolved,
            Err(reason) => return Decision::reply(call, reason),
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
        Decision::record(vec![CapEvent::SubAgent(Event::Requested {
            call: call.to_string(),
            pending: pending.clone(),
        })])
        .then(Act::Ask(self.ask(call, &pending)))
        // Parked, not answered: the session's answer is what this call's result
        // is made of, and it arrives on another message. The dangling
        // `tool_use` is what the reply fills in.
        .then(Act::Park {
            call: call.to_string(),
            note,
        })
    }

    /// The request a [`Pending`] names.
    ///
    /// One function and two callers — the spawn, and the re-ask on load —
    /// because the second has to send exactly what the first sent. Built from
    /// the journaled request plus this capability's own config, so nothing in
    /// it is minted twice.
    fn ask(&self, call: &str, pending: &Pending) -> SessionRequest {
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

    /// Everything asked for and never answered, asked again.
    ///
    /// A `Requested` still in the fold is a request the dead process may never
    /// have sent: the journal write comes first, so the window between it and
    /// the session hearing anything is exactly what this closes. Re-asked with
    /// the ids already recorded — the same call, the same child, the same agent
    /// — which is what lets the session tell a repeat from a new spawn.
    ///
    /// Nothing is journaled: the [`Event::Requested`] this reads is still the
    /// only fact, and a second copy of it would say a second spawn was wanted.
    fn reloaded(&self) -> Option<Decision> {
        if self.requested.is_empty() {
            return None;
        }
        Some(
            self.requested
                .iter()
                .fold(Decision::default(), |d, (call, pending)| {
                    d.then(Act::Ask(self.ask(call, pending)))
                }),
        )
    }

    /// The session answered a spawn this capability asked for.
    ///
    /// `None` for a call this capability never made, so a reply meant for
    /// another capability is not claimed by whichever sorted first.
    fn replied(&self, reply: &SessionReply) -> Option<Decision> {
        let child = self.requested.get(reply.call())?.child;
        Some(match reply {
            SessionReply::Done { call } => {
                Decision::record(vec![CapEvent::SubAgent(Event::Started {
                    call: call.clone(),
                })])
                .then(result(call, format!("Subagent spawned: {child}"), false))
            }
            // The refusal has to reach the model, and the call is parked — so
            // it is supplied as that call's result rather than as a fresh
            // answer. A refusal the model cannot see is a tool call that never
            // returns.
            SessionReply::Refused { call, reason } => {
                Decision::record(vec![CapEvent::SubAgent(Event::Dropped {
                    call: call.clone(),
                })])
                .then(result(call, reason.clone(), true))
            }
        })
    }

    /// A child moved.
    fn child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(o),
            } => {
                // Not one of mine: fall through as `None` rather than deliver
                // somebody else's report, so "addressed by owner" is enforced
                // by the same return type as "not my tool".
                self.outstanding.contains(child).then(|| {
                    self.deliver(
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
            // A run's outcome is the workflow capability's even when both are
            // held by the same agent. The outcome's kind and the owning
            // capability have to agree, and `None` is how they do.
            ChildMsg::Outcome {
                outcome: ChildOutcome::Workflow(_),
                ..
            } => None,
            // A child that died still owes its asker an answer: the agent is
            // sitting on a spawn it was told succeeded.
            ChildMsg::Failed { child, error } => self.outstanding.contains(child).then(|| {
                self.deliver(
                    *child,
                    failed_part(*child, child.to_string(), error.clone()),
                )
            }),
            // A worker is runnable the moment it is created; only a fork has a
            // seed that can land later.
            ChildMsg::Ready { .. } => None,
        }
    }

    /// Record the report and put it in this agent's own queue.
    ///
    /// Journaled *and* queued in one decision, so the acknowledgement and the
    /// delivery cannot land apart: a crash before the write replays as a report
    /// still outstanding, and it is delivered again.
    fn deliver(&self, child: RunnerId, part: SubAgentResultPart) -> Decision {
        Decision::record(vec![CapEvent::SubAgent(Event::Reported { child })]).then(Act::Enqueue {
            item: Incoming::SubAgent {
                id: child.to_string(),
                part: Box::new(part),
            },
        })
    }

    /// Only what is still running: a child that reported has been delivered
    /// into this agent's own transcript, so listing it again would show the
    /// model its own history back.
    fn render_status(&self) -> String {
        if self.outstanding.is_empty() {
            return "No subagents are running.".to_string();
        }
        let mut text = format!("{} subagent(s) running:", self.outstanding.len());
        for child in &self.outstanding {
            text.push_str(&format!("\n- {child}"));
        }
        text
    }
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

/// Supply a parked call's result, and start the turn that carries it.
fn result(call: &str, output: String, is_error: bool) -> Act {
    Act::Resume {
        results: vec![ToolResultInput {
            tool_call_id: call.to_string(),
            output,
            is_error,
        }],
    }
}

impl SubAgentCapability {
    /// Both tools, claimed by this capability's own layer rather than pushed as
    /// a layer in `setup`.
    ///
    /// That is the change the move made: a layer pushed there runs on the
    /// agent's task, where there is no mailbox to journal an intent on and no
    /// way to park the call while the session answers. A name claimed here is
    /// dispatched through [`Capability::handle`], which can do both.
    ///
    /// A budget the model can only ever be refused by advertises nothing. A
    /// tool like that is worse than no tool: it spends prompt on a capability
    /// that does not exist and invites a retry loop against a fixed number.
    ///
    /// The facts are what carry the agent catalogue, and this is the only reason
    /// [`Capability::layer`] takes them: the types a session can spawn are found
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
                move |input, to| {
                    CapCommand::SubAgent(
                        Command::Spawn {
                            input,
                            catalog: advertised.clone(),
                        },
                        to,
                    )
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
                |_input, to| CapCommand::SubAgent(Command::Status, to),
            ),
        ]
    }
}

#[async_trait::async_trait]
impl Capability for SubAgentCapability {
    fn name(&self) -> &'static str {
        "sub_agent"
    }

    fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(facts), mailbox)
    }

    fn command(&self, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::SubAgent(cmd, to) = cmd else {
            return None;
        };
        Some(match cmd {
            Command::Spawn { input, catalog } => self.spawn(&to.call, input, catalog.as_deref()),
            // A read, so it journals nothing: an event for it would grow the
            // log every time the model looked.
            Command::Status => Decision::reply(&to.call, self.render_status()),
        })
    }

    fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Reply(reply) => self.replied(reply),
            Msg::Child(m) => self.child(m),
            // The crash window: a spawn journaled and never answered is re-asked
            // with the ids the log already holds.
            Msg::Loaded => self.reloaded(),
            // Invariant 6. The turn is over and a report is still owed, so this
            // agent is not finished — it has work arriving that it has not
            // seen. `Act::Hold` rather than a claimed-but-empty decision: a
            // turn boundary is broadcast and merged, so claiming it is
            // invisible to the actor and only an act can carry the answer.
            Msg::Turn(TurnEvent::Ended) if self.holds_conclusion() => {
                Some(Decision::default().then(Act::Hold {
                    note: format!("{} subagent(s) still owe a report", self.outstanding.len()),
                }))
            }
            Msg::Turn(_) | Msg::Answer(_) | Msg::Woke { .. } | Msg::Concluded => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        // `let ... else` rather than a match with an arm per sibling: every
        // capability is offered every event, and listing the others here would
        // make adding one a change to all of them.
        let CapEvent::SubAgent(event) = event else {
            return;
        };
        match event {
            Event::Requested { call, pending } => {
                self.requested.insert(call.clone(), pending.clone());
            }
            Event::Started { call } => {
                if let Some(pending) = self.requested.remove(call) {
                    self.outstanding.insert(pending.child);
                }
            }
            Event::Dropped { call } => {
                self.requested.remove(call);
            }
            Event::Reported { child } => {
                self.outstanding.remove(child);
            }
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::SubAgent(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{
        FakeCapability, advertised_by, answering, facts, someone_elses, specs_of,
    };
    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::agent_loop::capabilities::testing::settings;
    use crate::sessions::runners::message::WorkflowOutcome;

    fn cap() -> SubAgentCapability {
        SubAgentCapability::new(settings(), 0)
    }

    /// A spawn as the layer builds it, under a load that found no agent types.
    fn spawn_cmd(input: serde_json::Value) -> CapCommand {
        spawn_under(input, &facts())
    }

    /// The same, under the facts a load actually found — which is what the
    /// layer captures, so a refusal names the list the model was shown.
    fn spawn_under(input: serde_json::Value, facts: &AgentFacts) -> CapCommand {
        CapCommand::SubAgent(
            Command::Spawn {
                input,
                catalog: shared_catalog(facts),
            },
            answering("t1"),
        )
    }

    fn spawn_call() -> CapCommand {
        spawn_cmd(json!({"label": "l", "task": "t"}))
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
    fn spawn_spec(c: &SubAgentCapability, facts: &AgentFacts) -> ToolSpec {
        specs_of(c, facts)
            .into_iter()
            .find(|t| t.name == SPAWN_TOOL)
            .expect("spawn_agent is advertised")
    }

    /// Fold a decision back into the capability that made it, exactly as the
    /// actor does — a capability that decided something has not yet changed.
    fn fold(c: &mut SubAgentCapability, d: &Decision) {
        for event in &d.events {
            c.apply(event);
        }
    }

    /// What the model was told, having checked that it was told rather than
    /// obeyed. A refusal is not a fact about the agent, so an event here would
    /// put a child that never existed in the log.
    fn refusal(d: &Decision) -> String {
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the agent"
        );
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        text.clone()
    }

    /// Ask for a worker and let the session say yes, which is the only way
    /// there is to an outstanding child.
    fn spawned(c: &mut SubAgentCapability) -> RunnerId {
        let d = c.command(&spawn_call()).expect("mine");
        fold(c, &d);
        let [
            Act::Ask(SessionRequest::StartRunner { id, .. }),
            Act::Park { .. },
        ] = d.acts.as_slice()
        else {
            panic!("expected an ask and a park, got {:?}", d.acts);
        };
        let child = *id;
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done { call: "t1".into() }))
            .expect("mine");
        fold(c, &d);
        child
    }

    /// The event and the request must name the same child. If they ever differ,
    /// the log records a child nothing created and the agent waits for ever.
    #[test]
    fn a_spawn_journals_and_asks_for_the_same_child() {
        let mut c = cap();
        let d = c
            .command(&spawn_cmd(
                json!({"label": "read the flake", "task": "look"}),
            ))
            .expect("mine");

        let [CapEvent::SubAgent(Event::Requested { call, pending })] = d.events.as_slice() else {
            panic!("expected one Requested event, got {:?}", d.events);
        };
        let child = &pending.child;
        assert_eq!(call, "t1");
        let [
            Act::Ask(SessionRequest::StartRunner {
                call,
                id,
                kind,
                args,
            }),
            Act::Park { call: parked, .. },
        ] = d.acts.as_slice()
        else {
            panic!("expected an ask and a park, got {:?}", d.acts);
        };
        assert_eq!(id, child, "the log records a child nothing was asked for");
        assert_eq!(call, "t1");
        assert_eq!(
            parked, "t1",
            "the call the session answers is the parked one"
        );
        assert_eq!(*kind, RunnerKind::SubAgent);
        let RunnerArgs::SubAgent { agent, label, .. } = args.as_ref() else {
            panic!("expected subagent args, got {args:?}");
        };
        // The worker's agent is decided here, with its runner, because
        // `spawn_agent`'s result names it — and it is its *own* id, not the
        // runner's. Two spaces on purpose: a workflow runner owns many agents,
        // so an equality that held for a worker would be false for a run.
        assert_ne!(agent.as_uuid(), child.as_uuid());
        assert_eq!(
            *agent, pending.agent,
            "the agent asked for is not the one journaled, so a replay would \
             mint a second worker the session cannot recognise"
        );
        assert_eq!(label, "read the flake");

        // Nothing is outstanding yet: the session has not said it exists.
        let pending = pending.clone();
        fold(&mut c, &d);
        assert!(c.outstanding.is_empty());
        assert_eq!(c.requested.get("t1"), Some(&pending));
    }

    /// The bound on nesting, and the one gate that stays agent-side: how deep
    /// this agent sits is fixed when it is equipped, so no round trip is needed
    /// to answer it. Without it a worker that spawns a worker is a machine that
    /// runs until something else stops it.
    #[test]
    fn a_spawn_at_the_depth_limit_is_refused_without_asking_the_session() {
        // The last depth that may still delegate, and the first that may not.
        let ok = SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH - 1)
            .command(&spawn_call())
            .expect("mine");
        assert!(matches!(ok.acts.first(), Some(Act::Ask(_))));

        let d = SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH)
            .command(&spawn_call())
            .expect("mine, refused or not");
        assert_eq!(refusal(&d), "max subagent depth 4 reached");
        assert!(
            !d.acts.iter().any(|a| matches!(a, Act::Ask(_))),
            "a refused spawn must not reach the session"
        );
    }

    /// **The concurrency cap is the session's now**, because it is a count over
    /// the whole tree and only the session holds the tree. So the capability
    /// asks, and the refusal comes back as a reply — which must still reach the
    /// model, against the call it parked, or the spawn never returns.
    #[test]
    fn a_cap_refusal_from_the_session_becomes_the_parked_calls_result() {
        let mut c = cap();
        let d = c.command(&spawn_call()).expect("mine");
        fold(&mut c, &d);

        let d = c
            .handle(&Msg::Reply(&SessionReply::Refused {
                call: "t1".into(),
                reason: "8 subagents already active".into(),
            }))
            .expect("the reply answers a call I made");
        let [Act::Resume { results }] = d.acts.as_slice() else {
            panic!(
                "a refusal the model cannot see is a call that never returns: {:?}",
                d.acts
            );
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id, "t1");
        assert_eq!(results[0].output, "8 subagents already active");
        assert!(results[0].is_error);

        // And the intent is retracted, so nothing re-asks for it on load and no
        // report is ever expected from a child that does not exist.
        fold(&mut c, &d);
        assert!(c.requested.is_empty());
        assert!(c.outstanding.is_empty());
    }

    /// The session said yes: the parked call gets the child's id, and only now
    /// is a report owed.
    #[test]
    fn a_started_child_answers_the_parked_call_and_becomes_outstanding() {
        let mut c = cap();
        let d = c.command(&spawn_call()).expect("mine");
        fold(&mut c, &d);
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done { call: "t1".into() }))
            .expect("mine");
        let [Act::Resume { results }] = d.acts.as_slice() else {
            panic!("expected the parked call to be answered, got {:?}", d.acts);
        };
        assert_eq!(results[0].tool_call_id, "t1");
        assert!(!results[0].is_error);
        assert!(
            results[0].output.starts_with("Subagent spawned: "),
            "the model is owed the id it can ask about: {}",
            results[0].output
        );

        fold(&mut c, &d);
        assert_eq!(c.outstanding.len(), 1);
        assert!(c.requested.is_empty(), "an answered request is over");
        assert!(
            results[0].output.contains(
                &c.outstanding
                    .iter()
                    .next()
                    .expect("one outstanding")
                    .to_string()
            ),
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
    #[test]
    fn a_spawn_the_session_never_answered_is_asked_again_on_load() {
        let mut c = cap();
        let d = c.command(&spawn_call()).expect("mine");
        fold(&mut c, &d);
        let [
            Act::Ask(SessionRequest::StartRunner { call, id, args, .. }),
            Act::Park { .. },
        ] = d.acts.as_slice()
        else {
            panic!("expected an ask and a park, got {:?}", d.acts);
        };
        let (first_call, child) = (call.clone(), *id);
        let RunnerArgs::SubAgent { agent, .. } = args.as_ref() else {
            panic!("expected subagent args, got {args:?}");
        };
        let worker = *agent;

        // The cut. Nothing is folded past the request, and what comes back is
        // read off the journal the way a new process reads it.
        let caps = Capabilities::new(vec![Box::new(c)]);
        let written = serde_json::to_string(&caps).expect("write");
        let reloaded: Capabilities = serde_json::from_str(&written).expect("read");

        let d = reloaded.broadcast(&Msg::Loaded);
        assert!(
            d.events.is_empty(),
            "a re-ask journals nothing: the Requested it reads is still the \
             only record, and a second copy would say a second spawn was wanted"
        );
        let [
            Act::Ask(SessionRequest::StartRunner {
                call,
                id,
                kind,
                args,
            }),
        ] = d.acts.as_slice()
        else {
            panic!("expected exactly one re-ask, got {:?}", d.acts);
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
        let mut c = cap();
        let _ = spawned(&mut c);
        assert!(
            c.handle(&Msg::Loaded).is_none(),
            "the session already created this child; asking again duplicates it"
        );

        // A refusal retracts the intent too, so a load after one asks for
        // nothing rather than re-running into the same budget.
        let mut c = cap();
        let d = c.command(&spawn_call()).expect("mine");
        fold(&mut c, &d);
        let d = c
            .handle(&Msg::Reply(&SessionReply::Refused {
                call: "t1".into(),
                reason: "8 subagents already active".into(),
            }))
            .expect("mine");
        fold(&mut c, &d);
        assert!(c.handle(&Msg::Loaded).is_none());
    }

    /// A reply for a call this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_call_i_never_made_is_not_mine() {
        let c = cap();
        assert!(
            c.handle(&Msg::Reply(&SessionReply::Done {
                call: "someone-else".into()
            }))
            .is_none()
        );
    }

    /// **A refused spawn must still be claimed.** Declining hands the call to
    /// the next capability, and the last one is the open-namespace sandbox that
    /// answers to every name — so the model would be answered by the sandbox
    /// and never learn it had hit a budget.
    #[test]
    fn a_refused_spawn_is_claimed_rather_than_left_to_the_sandbox() {
        let caps = Capabilities::new(vec![
            Box::new(SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH)),
            Box::new(FakeCapability::new(SPAWN_TOOL)),
        ]);
        let d = caps
            .dispatch(&spawn_call())
            .expect("a refused spawn that answers nobody");
        assert!(!refusal(&d).is_empty());
    }

    /// Arguments this capability cannot read are still *its* call to answer,
    /// for the same reason.
    #[test]
    fn a_malformed_spawn_is_refused_in_words_and_journals_nothing() {
        let d = cap()
            .command(&spawn_cmd(json!({"label": "l"})))
            .expect("the name is mine, so the mistake is mine to answer");
        assert!(refusal(&d).contains(SPAWN_TOOL));
    }

    /// A report goes into this agent's own queue, which is what replaced the
    /// session-side address: the capability belongs to the agent that asked.
    #[test]
    fn a_completed_report_is_queued_for_the_agent_that_asked() {
        let mut c = cap();
        let child = spawned(&mut c);

        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "found it".into(),
                }),
            }))
            .expect("mine");
        assert!(matches!(
            d.events.as_slice(),
            [CapEvent::SubAgent(Event::Reported { .. })]
        ));
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { id, part },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued report, got {:?}", d.acts);
        };
        assert_eq!(*id, child.to_string());
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "found it");
        assert_eq!(part.subagent_id, child.to_string());
        assert_eq!(part.label, "l");

        fold(&mut c, &d);
        assert!(c.outstanding.is_empty(), "a reported child owes nothing");
    }

    /// A failure is a report too. An agent blocked on a worker that died and
    /// was never told would wait for ever.
    #[test]
    fn a_failed_outcome_is_queued_as_a_failed_part() {
        let mut c = cap();
        let child = spawned(&mut c);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Failed {
                    label: "l".into(),
                    error: "it broke".into(),
                }),
            }))
            .expect("mine");
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { part, .. },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued report, got {:?}", d.acts);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "it broke");
    }

    /// A child that died before it said anything takes the same path: the asker
    /// is holding an id it was told was real.
    #[test]
    fn a_child_that_never_ran_is_reported_as_failed() {
        let mut c = cap();
        let child = spawned(&mut c);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Failed {
                child,
                error: "the create failed".into(),
            }))
            .expect("mine");
        assert!(matches!(
            d.events.as_slice(),
            [CapEvent::SubAgent(Event::Reported { .. })]
        ));
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { part, .. },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued report, got {:?}", d.acts);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "the create failed");
    }

    /// A child this capability did not create is not its business. Without the
    /// gate, a sibling's report would be queued for an agent that never asked.
    #[test]
    fn an_outcome_for_a_child_i_did_not_create_is_not_mine() {
        let c = cap();
        assert!(
            c.handle(&Msg::Child(&ChildMsg::Outcome {
                child: RunnerId::new_v4(),
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "r".into(),
                }),
            }))
            .is_none()
        );
    }

    /// A run's outcome is the workflow capability's, even for a child id this
    /// one holds. Two capabilities must never both plausibly claim an outcome.
    #[test]
    fn a_workflow_outcome_is_never_mine() {
        let mut c = cap();
        let child = spawned(&mut c);
        assert!(
            c.handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: json!("done")
                }),
            }))
            .is_none()
        );
        // And a worker is runnable the moment it exists, so `Ready` is a fork's
        // message and not this one's.
        assert!(c.handle(&Msg::Child(&ChildMsg::Ready { child })).is_none());
    }

    /// **Invariant 6.** A turn ending while a report is still owed does not
    /// finish this agent: the child's report is work it has not seen yet, and
    /// concluding here is how a superseded step lands a second conclusion on an
    /// index the run already routed past.
    #[test]
    fn a_turn_ending_with_an_outstanding_child_holds_the_conclusion() {
        let mut c = cap();
        let child = spawned(&mut c);
        assert!(c.holds_conclusion());
        assert!(
            c.handle(&Msg::Turn(TurnEvent::Ended)).is_some(),
            "a turn ending with a report still owed must not let the agent finish"
        );

        // The report lands, and the very next boundary lets it go.
        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "done".into(),
                }),
            }))
            .expect("mine");
        fold(&mut c, &d);
        assert!(!c.holds_conclusion());
        assert!(
            c.handle(&Msg::Turn(TurnEvent::Ended)).is_none(),
            "an agent owed nothing has no opinion about its turn ending"
        );
    }

    /// A child merely *asked for* holds nothing: the session has not said it
    /// exists, and the spawn call is parked, so the agent is not finishing
    /// anyway. Reading `requested` here would hold a conclusion for a child
    /// that was refused.
    #[test]
    fn a_requested_child_does_not_hold_the_conclusion() {
        let mut c = cap();
        let d = c.command(&spawn_call()).expect("mine");
        fold(&mut c, &d);
        assert!(!c.requested.is_empty());
        assert!(!c.holds_conclusion());
        assert!(c.handle(&Msg::Turn(TurnEvent::Ended)).is_none());
    }

    /// Only the *end* of a turn is the boundary invariant 6 reads. A turn
    /// beginning, failing or being cancelled says nothing about whether this
    /// agent may finish.
    #[test]
    fn no_other_turn_boundary_is_this_capabilitys_business() {
        let mut c = cap();
        let _ = spawned(&mut c);
        for boundary in [TurnEvent::Began, TurnEvent::Failed, TurnEvent::Cancelled] {
            assert!(
                c.handle(&Msg::Turn(boundary)).is_none(),
                "{boundary:?} was claimed"
            );
        }
    }

    /// A status read journals nothing, and lists only what is still going: a
    /// child that reported is already in this agent's own transcript.
    #[test]
    fn status_lists_the_outstanding_children_and_journals_nothing() {
        let mut c = cap();
        let child = spawned(&mut c);
        let d = c
            .command(&CapCommand::SubAgent(Command::Status, answering("t1")))
            .expect("mine");
        assert!(d.events.is_empty());
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(text.contains(&child.to_string()));

        c.apply(&CapEvent::SubAgent(Event::Reported { child }));
        let d = c
            .command(&CapCommand::SubAgent(Command::Status, answering("t1")))
            .expect("mine");
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(!text.contains(&child.to_string()));
    }

    /// Both tools, claimed by this capability's own layer — which is what
    /// routes the call to the mailbox, where the intent can be journaled and the
    /// call parked.
    #[test]
    fn it_advertises_both_tools() {
        assert_eq!(
            advertised_by(&cap(), &facts()),
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
        let spawn = spawn_spec(&cap(), &facts);
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
            let spawn = spawn_spec(&cap(), &facts);
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
        let d = cap()
            .command(&spawn_under(
                json!({"label": "l", "task": "t", "agent_type": "reviewer"}),
                &installed,
            ))
            .expect("the name is mine, so the mistake is mine to answer");
        assert_eq!(
            refusal(&d),
            "no agent type 'reviewer'; installed types are code-reviewer, scout"
        );
        assert!(
            !d.acts.iter().any(|a| matches!(a, Act::Ask(_))),
            "a refused spawn must not reach the session"
        );

        // With nothing installed the refusal says so, rather than naming an
        // empty list.
        let d = cap()
            .command(&spawn_cmd(
                json!({"label": "l", "task": "t", "agent_type": "reviewer"}),
            ))
            .expect("mine");
        assert_eq!(
            refusal(&d),
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
            let d = cap().command(&spawn_under(input, &facts)).expect("mine");
            let [CapEvent::SubAgent(Event::Requested { pending, .. })] = d.events.as_slice() else {
                panic!("expected one Requested event, got {:?}", d.events);
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
        assert!(advertised_by(&SubAgentCapability::new(zero, 0), &facts()).is_empty());
        assert!(
            advertised_by(
                &SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH),
                &facts()
            )
            .is_empty()
        );
    }

    /// `outstanding` is what invariant 6 and every delivery are decided from,
    /// so losing it in the journal loses the agent: the report arrives on a
    /// process that has since rehydrated the session.
    #[test]
    fn the_outstanding_children_survive_a_slice_round_trip() {
        let mut c = cap();
        let child = spawned(&mut c);
        let caps = Capabilities::new(vec![Box::new(c)]);

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::SubAgent(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            back.outstanding.into_iter().collect::<Vec<_>>(),
            vec![child],
            "the reload was rebuilt from config and lost the outstanding child"
        );
        assert_eq!(back.depth, 0, "the gate this agent carries is config too");
    }

    /// Everything else falls through, so the offer reaches the capability that
    /// does own it.
    #[test]
    fn another_message_is_not_mine() {
        let c = cap();
        assert!(c.command(&someone_elses()).is_none());
        assert!(c.handle(&Msg::Answer(&[])).is_none());
    }
}
