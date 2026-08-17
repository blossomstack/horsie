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
    Act, CapEvent, CapSlice, Capability, Decision, Msg, SessionReply, SessionRequest, TurnEvent,
};
use crate::agent_loop::Incoming;
use crate::sessions::runners::action::RunnerArgs;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::message::{ChildMsg, ChildOutcome, SubAgentOutcome, ToolCall};
use crate::sessions::spec::AgentSettings;
use crate::sessions::subagents::MAX_SUBAGENT_DEPTH;
use horsie_agentcore::ToolSpec;
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

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
    /// Spawns asked for and not yet answered: the model's call, and the child
    /// it named.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent the session can be asked about again — and it dedupes by the
    /// same call id rather than starting a second child.
    pub requested: BTreeMap<String, RunnerId>,
    /// Children that exist and still owe a report.
    ///
    /// A set rather than the session-side map to an `AgentId`: the capability
    /// belongs to the agent that asked, so the report goes into its own queue
    /// and there is no address to keep. It is also the single fact behind both
    /// questions anyone asks about delegated work — is a report still owed, and
    /// may this agent finish (invariant 6).
    pub outstanding: BTreeSet<RunnerId>,
}

/// What this capability records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A spawn was asked of the session, and this is the child it named.
    Requested { call: String, child: RunnerId },
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
    fn spawn(&self, call: &ToolCall) -> Decision {
        let req: Request = match serde_json::from_value(call.input.clone()) {
            Ok(req) => req,
            // A capability that owns a tool name owns every call to it,
            // including the malformed ones — see the module doc on why this is
            // answered rather than declined.
            Err(e) => {
                return Decision::reply(
                    &call.id,
                    format!("`{SPAWN_TOOL}` was called with arguments it cannot read: {e}"),
                );
            }
        };
        // `depth` is the *asking* agent's, so the first worker of a
        // conversation is spawned from depth 0 and lands at 1 — which is why
        // the bound is `>=` and not `>`. The concurrency cap is not checked
        // here: see the module doc.
        if self.depth > MAX_SUBAGENT_DEPTH {
            return Decision::reply(
                &call.id,
                format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached"),
            );
        }
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
        let child = RunnerId::new_v4();
        let agent = AgentId::new_v4();
        let label = req.label.clone();
        Decision::record(vec![CapEvent::SubAgent(Event::Requested {
            call: call.id.clone(),
            child,
        })])
        .then(Act::Ask(SessionRequest::StartRunner {
            call: call.id.clone(),
            id: child,
            kind: RunnerKind::SubAgent,
            args: Box::new(RunnerArgs::SubAgent {
                agent,
                label: req.label,
                task: req.task,
                agent_type: req.agent_type,
                settings: Box::new(self.child_settings.clone()),
            }),
        }))
        // Parked, not answered: the session's answer is what this call's result
        // is made of, and it arrives on another message. The dangling
        // `tool_use` is what the reply fills in.
        .then(Act::Park {
            call: call.id.clone(),
            note: format!("spawning subagent {label}"),
        })
    }

    /// The session answered a spawn this capability asked for.
    ///
    /// `None` for a call this capability never made, so a reply meant for
    /// another capability is not claimed by whichever sorted first.
    fn replied(&self, reply: &SessionReply) -> Option<Decision> {
        let child = *self.requested.get(reply.call())?;
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

#[async_trait::async_trait]
impl Capability for SubAgentCapability {
    fn name(&self) -> &'static str {
        "sub_agent"
    }

    /// Both tools, advertised here rather than pushed as a toolbox layer.
    ///
    /// That is the change the move made: a layer runs on the agent's task,
    /// where there is no mailbox to journal an intent on and no way to park the
    /// call while the session answers. A tool named here is dispatched through
    /// [`Capability::handle`], which can do both.
    ///
    /// A budget the model can only ever be refused by advertises nothing. A
    /// tool like that is worse than no tool: it spends prompt on a capability
    /// that does not exist and invites a retry loop against a fixed number.
    fn tools(&self) -> Vec<ToolSpec> {
        if self.child_settings.max_subagents() == 0 || self.depth >= MAX_SUBAGENT_DEPTH {
            return Vec::new();
        }
        vec![
            ToolSpec {
                name: SPAWN_TOOL.to_string(),
                description: "Spawn a subagent to work on a task independently and in parallel. \
                    Returns immediately with the subagent's id; its result or failure is \
                    automatically delivered back to you as a message. Continue with independent \
                    work, or wait if none remains; do not poll subagent_status or call it \
                    repeatedly. Spawning fails when the session's subagent limits (depth or \
                    concurrency) are reached."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["label", "task"],
                    "properties": {
                        "label": {
                            "type": "string",
                            "description": "A short human-readable label for the subagent (a few \
                                words)."
                        },
                        "task": {
                            "type": "string",
                            "description": "The complete, self-contained task for the subagent. \
                                It inherits your model and tools but not your conversation — \
                                include everything it needs to know."
                        }
                    }
                }),
            },
            ToolSpec {
                name: STATUS_TOOL.to_string(),
                description: "Inspect subagent status only for a user-requested progress update \
                    or to diagnose a suspected result-delivery problem. Do not poll or call this \
                    tool repeatedly: terminal results and failures are automatically delivered \
                    to you as messages. Lists the subagents you spawned that are still running."
                    .to_string(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Tool(call) if call.name == SPAWN_TOOL => Some(self.spawn(call)),
            // A read, so it journals nothing: an event for it would grow the
            // log every time the model looked.
            Msg::Tool(call) if call.name == STATUS_TOOL => {
                Some(Decision::reply(&call.id, self.render_status()))
            }
            Msg::Reply(reply) => self.replied(reply),
            Msg::Child(m) => self.child(m),
            // Invariant 6. The turn is over and a report is still owed, so this
            // agent is not finished — it has work arriving that it has not
            // seen. Claimed rather than declined so the boundary carries the
            // answer; the conclusion is held until the last child reports.
            Msg::Turn(TurnEvent::Ended) if self.holds_conclusion() => Some(Decision::noop()),
            Msg::Tool(_) | Msg::Command(_) | Msg::Turn(_) | Msg::Answer(_) => None,
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
            Event::Requested { call, child } => {
                self.requested.insert(call.clone(), *child);
            }
            Event::Started { call } => {
                if let Some(child) = self.requested.remove(call) {
                    self.outstanding.insert(child);
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
    use super::super::testing::FakeCapability;
    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::sessions::runners::capabilities::testing::settings;
    use crate::sessions::runners::message::WorkflowOutcome;

    fn cap() -> SubAgentCapability {
        SubAgentCapability::new(settings(), 0)
    }

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
        }
    }

    fn spawn_call() -> ToolCall {
        call(SPAWN_TOOL, json!({"label": "l", "task": "t"}))
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
        let d = c.handle(&Msg::Tool(&spawn_call())).expect("mine");
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
            .handle(&Msg::Tool(&call(
                SPAWN_TOOL,
                json!({"label": "read the flake", "task": "look"}),
            )))
            .expect("mine");

        let [CapEvent::SubAgent(Event::Requested { call, child })] = d.events.as_slice() else {
            panic!("expected one Requested event, got {:?}", d.events);
        };
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
        assert_eq!(label, "read the flake");

        // Nothing is outstanding yet: the session has not said it exists.
        fold(&mut c, &d);
        assert!(c.outstanding.is_empty());
        assert_eq!(c.requested.get("t1"), Some(child));
    }

    /// The bound on nesting, and the one gate that stays agent-side: how deep
    /// this agent sits is fixed when it is equipped, so no round trip is needed
    /// to answer it. Without it a worker that spawns a worker is a machine that
    /// runs until something else stops it.
    #[test]
    fn a_spawn_at_the_depth_limit_is_refused_without_asking_the_session() {
        // The last depth that may still delegate, and the first that may not.
        let ok = SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH - 1)
            .handle(&Msg::Tool(&spawn_call()))
            .expect("mine");
        assert!(matches!(ok.acts.first(), Some(Act::Ask(_))));

        let d = SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH)
            .handle(&Msg::Tool(&spawn_call()))
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
        let d = c.handle(&Msg::Tool(&spawn_call())).expect("mine");
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
        let d = c.handle(&Msg::Tool(&spawn_call())).expect("mine");
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
        let taken = caps
            .iter()
            .find_map(|c| c.handle(&Msg::Tool(&spawn_call())).map(|d| (c.name(), d)));
        let Some(("sub_agent", d)) = taken else {
            panic!("the sandbox layer swallowed the spawn: {taken:?}");
        };
        assert!(!refusal(&d).is_empty());
    }

    /// Arguments this capability cannot read are still *its* call to answer,
    /// for the same reason.
    #[test]
    fn a_malformed_spawn_is_refused_in_words_and_journals_nothing() {
        let d = cap()
            .handle(&Msg::Tool(&call(SPAWN_TOOL, json!({"label": "l"}))))
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
        let d = c.handle(&Msg::Tool(&spawn_call())).expect("mine");
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
            .handle(&Msg::Tool(&call(STATUS_TOOL, json!({}))))
            .expect("mine");
        assert!(d.events.is_empty());
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(text.contains(&child.to_string()));

        c.apply(&CapEvent::SubAgent(Event::Reported { child }));
        let d = c
            .handle(&Msg::Tool(&call(STATUS_TOOL, json!({}))))
            .expect("mine");
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(!text.contains(&child.to_string()));
    }

    /// Both tools, advertised through `tools()` rather than a toolbox layer —
    /// which is what routes the call to the mailbox, where the intent can be
    /// journaled and the call parked.
    #[test]
    fn it_advertises_both_tools() {
        assert_eq!(
            cap()
                .tools()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>(),
            vec![SPAWN_TOOL, STATUS_TOOL]
        );
    }

    /// A budget the model can only ever be refused by advertises no tool at
    /// all, so it never meets one that cannot work.
    #[test]
    fn a_spent_budget_advertises_nothing() {
        let mut zero = settings();
        zero.max_concurrent_subagents = Some(0);
        assert!(SubAgentCapability::new(zero, 0).tools().is_empty());
        assert!(
            SubAgentCapability::new(settings(), MAX_SUBAGENT_DEPTH)
                .tools()
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
        assert!(c.handle(&Msg::Tool(&call("bash", json!({})))).is_none());
        assert!(
            c.handle(&Msg::Command(&crate::sessions::runners::message::Command {
                name: "fork".into(),
                args: String::new(),
            }))
            .is_none()
        );
        assert!(c.handle(&Msg::Answer(&[])).is_none());
    }
}
