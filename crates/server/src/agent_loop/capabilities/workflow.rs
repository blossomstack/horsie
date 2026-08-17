//! `invoke_workflow` and `workflow_status`: an agent starting a run, and
//! reading back the ones still going.
//!
//! A run invoked this way is a runner inside the same session, parented on the
//! agent that asked, so a session may hold any number of them at once — the
//! shape this replaces had one `Option<WorkflowRunState>` on the session and
//! inferred a subagent's owning tree from "which step is in flight", an
//! inference with no answer once two runs are live.
//!
//! Shaped exactly like [`super::sub_agent`], and for the same reason: a run
//! that reports owes its output to the agent that invoked it,
//! [`WorkflowCapability::outstanding`] is the one fact saying whether it has
//! been told, and the record and the delivery are one decision, so a crash
//! before the write replays as an output still owed.
//!
//! Invariant 6 applies here too: a run in flight is an outstanding child, and
//! an agent may not conclude while it has one.
//!
//! The two capabilities do not overlap. A [`ChildOutcome::SubAgent`] is
//! declined here even for a child id this capability holds: the outcome's kind
//! and the owning capability have to agree, and `None` is how they say so.
//!
//! # Why it advertises a tool now
//!
//! Its session-side twin equipped nothing, because `invoke_workflow` had no
//! toolbox behind it and advertising a tool before there is one to execute is
//! how a model learns to call something that answers "no such tool". That is
//! no longer true: a tool named in [`Capability::tools`] is dispatched through
//! [`Capability::handle`] on this actor, which is the half that was missing.

use super::{
    Act, CapEvent, CapSlice, Capability, Decision, Msg, SessionReply, SessionRequest, TurnEvent,
};
use crate::agent_loop::Incoming;
use crate::sessions::runners::action::{RunnerArgs, WorkflowSource};
use crate::sessions::runners::ids::{RunnerId, RunnerKind};
use crate::sessions::runners::message::{ChildMsg, ChildOutcome, ToolCall, WorkflowOutcome};
use horsie_agentcore::ToolSpec;
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The tool that starts a run.
pub const INVOKE_TOOL: &str = "invoke_workflow";
/// The tool that reads back what is still running.
pub const STATUS_TOOL: &str = "workflow_status";

/// One agent's workflow runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowCapability {
    /// Runs asked of the session and not yet answered: the model's call, and
    /// the run it named.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent the session can be asked about again — and it dedupes by the
    /// same call id rather than starting a second run.
    pub requested: BTreeMap<String, RunnerId>,
    /// Runs that exist and still owe their output.
    ///
    /// A set rather than the session-side map to an `AgentId`: the capability
    /// belongs to the agent that invoked, so the output goes into its own queue
    /// and there is no address to keep.
    pub outstanding: BTreeSet<RunnerId>,
}

/// What this capability records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A run was asked of the session, and this is the runner it named.
    Requested { call: String, child: RunnerId },
    /// The session created it, so an output is now owed.
    Started { call: String },
    /// The session would not create it. Journaled because the
    /// [`Event::Requested`] before it was — this retracts a fact, and is not
    /// itself the refusal.
    Dropped { call: String },
    /// This run's output reached the queue.
    Reported { child: RunnerId },
}

/// The tool's arguments: exactly what the model writes, and nothing else.
///
/// The model names a workflow; it never writes a graph. Turning that name into
/// a definition is a database read, and a database read may not happen on a
/// mailbox — so this capability asks for [`WorkflowSource::Named`] and the
/// session resolves it while performing the create. That keeps the split every
/// other request makes rather than smuggling a resolved graph through the
/// tool's own arguments, where it would sit in a field the model must never
/// fill.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub workflow: String,
    pub input: String,
}

impl WorkflowCapability {
    /// Whether this agent is still owed an output it cannot produce yet.
    ///
    /// Invariant 6, named: an agent may not conclude while it has outstanding
    /// children, and a run in flight is one.
    #[must_use]
    pub fn holds_conclusion(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// The model called `invoke_workflow`.
    fn invoke(&self, call: &ToolCall) -> Decision {
        let req: Request = match serde_json::from_value(call.input.clone()) {
            Ok(req) => req,
            // A capability that owns a tool name owns every call to it,
            // including the malformed ones. Declining would hand a mistyped
            // `invoke_workflow` to the open-namespace sandbox, which claims
            // anything — so the model's mistake would be silently absorbed
            // instead of corrected.
            Err(e) => {
                return Decision::reply(
                    &call.id,
                    format!("`{INVOKE_TOOL}` was called with arguments it cannot read: {e}"),
                );
            }
        };
        // Minted here, not in `apply`: a decision may be non-deterministic, a
        // fold may not. The event and the request then name the same run, and
        // replay lands the id the log has.
        let child = RunnerId::new_v4();
        let workflow = req.workflow.clone();
        Decision::record(vec![CapEvent::Workflow(Event::Requested {
            call: call.id.clone(),
            child,
        })])
        .then(Act::Ask(SessionRequest::StartRunner {
            call: call.id.clone(),
            id: child,
            kind: RunnerKind::Workflow,
            args: Box::new(RunnerArgs::Workflow {
                source: WorkflowSource::Named(req.workflow),
                input: req.input,
            }),
        }))
        // Parked, not answered: the session's answer is what this call's result
        // is made of, and it arrives on another message.
        .then(Act::Park {
            call: call.id.clone(),
            note: format!("starting workflow {workflow}"),
        })
    }

    /// The session answered a run this capability asked for.
    fn replied(&self, reply: &SessionReply) -> Option<Decision> {
        let child = *self.requested.get(reply.call())?;
        Some(match reply {
            SessionReply::Done { call } => {
                Decision::record(vec![CapEvent::Workflow(Event::Started {
                    call: call.clone(),
                })])
                .then(result(
                    call,
                    format!("Workflow run started: {child}"),
                    false,
                ))
            }
            // The refusal has to reach the model, and the call is parked — so
            // it is supplied as that call's result rather than as a fresh
            // answer. A refusal the model cannot see is a tool call that never
            // returns.
            SessionReply::Refused { call, reason } => {
                Decision::record(vec![CapEvent::Workflow(Event::Dropped {
                    call: call.clone(),
                })])
                .then(result(call, reason.clone(), true))
            }
        })
    }

    /// A run moved.
    fn child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(o),
            } => {
                // Not one of mine: `None` rather than a delivery for an agent
                // that never invoked anything.
                self.outstanding.contains(child).then(|| {
                    self.deliver(
                        *child,
                        match o {
                            WorkflowOutcome::Finished { output } => SubAgentResultPart {
                                subagent_id: child.to_string(),
                                label: "workflow".to_string(),
                                status: "completed".to_string(),
                                text: render(output),
                                spawned_at_ms: 0,
                                ended_at_ms: 0,
                            },
                            WorkflowOutcome::Failed { error } => failed_part(*child, error.clone()),
                        },
                    )
                })
            }
            // A worker's report is the subagent capability's, even when this
            // capability holds that child id. Two capabilities must never both
            // plausibly claim one outcome.
            ChildMsg::Outcome {
                outcome: ChildOutcome::SubAgent(_),
                ..
            } => None,
            ChildMsg::Failed { child, error } => self
                .outstanding
                .contains(child)
                .then(|| self.deliver(*child, failed_part(*child, error.clone()))),
            // A run is runnable the moment it is created; only a fork has a
            // seed that can land later.
            ChildMsg::Ready { .. } => None,
        }
    }

    /// Record the output and put it in this agent's own queue.
    fn deliver(&self, child: RunnerId, part: SubAgentResultPart) -> Decision {
        Decision::record(vec![CapEvent::Workflow(Event::Reported { child })]).then(Act::Enqueue {
            item: Incoming::SubAgent {
                id: child.to_string(),
                part: Box::new(part),
            },
        })
    }

    /// Only what is still running: a run that reported has already been
    /// delivered into this agent's transcript.
    fn render_status(&self) -> String {
        if self.outstanding.is_empty() {
            return "No workflow runs are in flight.".to_string();
        }
        let mut text = format!("{} workflow run(s) in flight:", self.outstanding.len());
        for child in &self.outstanding {
            text.push_str(&format!("\n- {child}"));
        }
        text
    }
}

/// A failed run, in the shape the invoking agent's inbox takes — the same part
/// a subagent's report arrives as, because to the agent that asked they are the
/// same thing: work it delegated, come back.
///
/// The timestamps are zero because this capability holds neither; they live on
/// the run's `RunnerRecord`, and a client shows no duration rather than one
/// that never happened.
fn failed_part(child: RunnerId, error: String) -> SubAgentResultPart {
    SubAgentResultPart {
        subagent_id: child.to_string(),
        label: "workflow".to_string(),
        status: "failed".to_string(),
        text: error,
        spawned_at_ms: 0,
        ended_at_ms: 0,
    }
}

/// A run's output is a `Value`, and a step's output is usually already text.
/// Rendering a JSON string as `"…"` would hand the model its own quotes back.
fn render(output: &Value) -> String {
    if let Value::String(s) = output {
        return s.clone();
    }
    serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
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
impl Capability for WorkflowCapability {
    fn name(&self) -> &'static str {
        "workflow"
    }

    /// Both tools, advertised here rather than pushed as a toolbox layer — see
    /// the module doc for why that is what made advertising them honest.
    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: INVOKE_TOOL.to_string(),
                description: "Start a run of a workflow already defined on this server. Returns \
                    immediately; the run's final output, or its failure, is automatically \
                    delivered back to you as a message. Carry on with independent work meanwhile, \
                    and use workflow_status only when asked for progress or when a result seems \
                    lost — never as a poll."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["workflow", "input"],
                    "properties": {
                        "workflow": {
                            "type": "string",
                            "description": "The name of the workflow to run."
                        },
                        "input": {
                            "type": "string",
                            "description": "The run's input, complete and self-contained: a run \
                                does not see this conversation."
                        }
                    }
                }),
            },
            ToolSpec {
                name: STATUS_TOOL.to_string(),
                description: "List the workflow runs you started that are still in flight. Do \
                    not poll or call this tool repeatedly: a run's output and its failures are \
                    automatically delivered to you as messages."
                    .to_string(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Tool(call) if call.name == INVOKE_TOOL => Some(self.invoke(call)),
            // A read, so it journals nothing: an event for it would grow the
            // log every time the model looked.
            Msg::Tool(call) if call.name == STATUS_TOOL => {
                Some(Decision::reply(&call.id, self.render_status()))
            }
            Msg::Reply(reply) => self.replied(reply),
            Msg::Child(m) => self.child(m),
            // Invariant 6: a run in flight is an outstanding child, and its
            // output is work this agent has not seen yet. `Act::Hold` because a
            // turn boundary is broadcast and merged — see `sub_agent`.
            Msg::Turn(TurnEvent::Ended) if self.holds_conclusion() => {
                Some(Decision::default().then(Act::Hold {
                    note: format!("{} workflow run(s) still in flight", self.outstanding.len()),
                }))
            }
            Msg::Tool(_) | Msg::Command(_) | Msg::Turn(_) | Msg::Answer(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        // `let ... else` rather than a match with an arm per sibling: every
        // capability is offered every event, and listing the others here would
        // make adding one a change to all of them.
        let CapEvent::Workflow(event) = event else {
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
        CapSlice::Workflow(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::sessions::runners::message::SubAgentOutcome;

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
        }
    }

    fn invoke_call() -> ToolCall {
        call(
            INVOKE_TOOL,
            json!({"workflow": "release", "input": "cut 1.2.0"}),
        )
    }

    fn fold(c: &mut WorkflowCapability, d: &Decision) {
        for event in &d.events {
            c.apply(event);
        }
    }

    /// Invoke, and let the session say yes — the only way there is to a run in
    /// flight.
    fn invoked(c: &mut WorkflowCapability) -> RunnerId {
        let d = c.handle(&Msg::Tool(&invoke_call())).expect("mine");
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

    /// The event and the request must name the same run, or the log records a
    /// run nothing created and the agent waits for ever. The args carry the
    /// name the model wrote; the session resolves it while performing the
    /// create, because a database read may not happen on a mailbox.
    #[test]
    fn an_invocation_journals_and_asks_for_the_same_run() {
        let mut c = WorkflowCapability::default();
        let d = c.handle(&Msg::Tool(&invoke_call())).expect("mine");

        let [CapEvent::Workflow(Event::Requested { call, child })] = d.events.as_slice() else {
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
        assert_eq!(id, child, "the log records a run nothing was asked for");
        assert_eq!(call, "t1");
        assert_eq!(parked, "t1");
        assert_eq!(*kind, RunnerKind::Workflow);
        let RunnerArgs::Workflow { source, input } = args.as_ref() else {
            panic!("expected workflow args, got {args:?}");
        };
        let WorkflowSource::Named(name) = source else {
            panic!("a tool call names a workflow; it never writes a graph");
        };
        assert_eq!(name, "release");
        assert_eq!(input, "cut 1.2.0");

        fold(&mut c, &d);
        assert!(c.outstanding.is_empty(), "the session has not said yes yet");
    }

    /// Arguments this capability cannot read are still *its* call to answer.
    /// Falling through would hand a mistyped `invoke_workflow` to the
    /// open-namespace sandbox, which claims anything — so the model's mistake
    /// would be silently absorbed instead of corrected.
    #[test]
    fn a_malformed_invocation_is_refused_in_words_and_journals_nothing() {
        let d = WorkflowCapability::default()
            .handle(&Msg::Tool(&call(INVOKE_TOOL, json!({"workflow": "nope"}))))
            .expect("the name is mine, so the mistake is mine to answer");
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the agent"
        );
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(
            text.contains(INVOKE_TOOL),
            "the reply names the tool: {text}"
        );
    }

    /// The session said yes: the parked call gets the run's id, and only now is
    /// an output owed.
    #[test]
    fn a_started_run_answers_the_parked_call_and_becomes_outstanding() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        assert_eq!(
            c.outstanding.iter().copied().collect::<Vec<_>>(),
            vec![child]
        );
        assert!(c.requested.is_empty(), "an answered request is over");
    }

    /// A refusal has to reach the model against the call it parked, or the
    /// invocation never returns.
    #[test]
    fn a_refusal_from_the_session_becomes_the_parked_calls_result() {
        let mut c = WorkflowCapability::default();
        let d = c.handle(&Msg::Tool(&invoke_call())).expect("mine");
        fold(&mut c, &d);

        let d = c
            .handle(&Msg::Reply(&SessionReply::Refused {
                call: "t1".into(),
                reason: "no workflow named `release`".into(),
            }))
            .expect("the reply answers a call I made");
        let [Act::Resume { results }] = d.acts.as_slice() else {
            panic!("expected the parked call to be answered, got {:?}", d.acts);
        };
        assert_eq!(results[0].tool_call_id, "t1");
        assert_eq!(results[0].output, "no workflow named `release`");
        assert!(results[0].is_error);

        fold(&mut c, &d);
        assert!(c.requested.is_empty());
        assert!(c.outstanding.is_empty());
    }

    /// A reply for a call this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_call_i_never_made_is_not_mine() {
        assert!(
            WorkflowCapability::default()
                .handle(&Msg::Reply(&SessionReply::Done {
                    call: "someone-else".into()
                }))
                .is_none()
        );
    }

    /// The run's terminal output reaches the agent that invoked it, rendered as
    /// text rather than as a quoted JSON string.
    #[test]
    fn a_finished_run_queues_its_output_for_the_invoker() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: json!("shipped"),
                }),
            }))
            .expect("mine");
        assert!(matches!(
            d.events.as_slice(),
            [CapEvent::Workflow(Event::Reported { .. })]
        ));
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { id, part },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued output, got {:?}", d.acts);
        };
        assert_eq!(*id, child.to_string());
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "shipped", "the model was handed its own quotes");

        fold(&mut c, &d);
        assert!(c.outstanding.is_empty());
    }

    /// A run that failed is still an answer. An agent blocked on one that died
    /// and was never told would wait for ever.
    #[test]
    fn a_failed_run_is_queued_as_a_failed_part() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Failed {
                    error: "step 2 failed".into(),
                }),
            }))
            .expect("mine");
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { part, .. },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued output, got {:?}", d.acts);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "step 2 failed");
    }

    /// A run that never started takes the same path.
    #[test]
    fn a_run_that_never_started_is_reported_as_failed() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Failed {
                child,
                error: "the create failed".into(),
            }))
            .expect("mine");
        let [
            Act::Enqueue {
                item: Incoming::SubAgent { part, .. },
            },
        ] = d.acts.as_slice()
        else {
            panic!("expected a queued output, got {:?}", d.acts);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "the create failed");
    }

    /// The outcome's kind and the owning capability must agree. A subagent's
    /// report is declined *even for a child id this capability holds* — which
    /// is the only way two capabilities on one agent cannot both claim it.
    #[test]
    fn a_subagent_outcome_is_never_mine_even_for_a_child_i_hold() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        assert!(
            c.handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "r".into(),
                }),
            }))
            .is_none()
        );
        assert!(c.handle(&Msg::Child(&ChildMsg::Ready { child })).is_none());
    }

    /// A run this capability did not invoke is not its business.
    #[test]
    fn an_outcome_for_a_run_i_did_not_invoke_is_not_mine() {
        assert!(
            WorkflowCapability::default()
                .handle(&Msg::Child(&ChildMsg::Outcome {
                    child: RunnerId::new_v4(),
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                        output: json!("done"),
                    }),
                }))
                .is_none()
        );
    }

    /// **Invariant 6.** A turn ending while a run is in flight does not finish
    /// this agent: the run's output is work it has not seen yet.
    #[test]
    fn a_turn_ending_with_a_run_in_flight_holds_the_conclusion() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        assert!(c.holds_conclusion());
        assert!(
            c.handle(&Msg::Turn(TurnEvent::Ended)).is_some(),
            "a turn ending with an output still owed must not let the agent finish"
        );

        let d = c
            .handle(&Msg::Child(&ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: json!("done"),
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

    /// Only the *end* of a turn is the boundary invariant 6 reads.
    #[test]
    fn no_other_turn_boundary_is_this_capabilitys_business() {
        let mut c = WorkflowCapability::default();
        let _ = invoked(&mut c);
        for boundary in [TurnEvent::Began, TurnEvent::Failed, TurnEvent::Cancelled] {
            assert!(
                c.handle(&Msg::Turn(boundary)).is_none(),
                "{boundary:?} was claimed"
            );
        }
    }

    /// A status read journals nothing, and lists only what is still going.
    #[test]
    fn status_lists_the_runs_in_flight_and_journals_nothing() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        let d = c
            .handle(&Msg::Tool(&call(STATUS_TOOL, json!({}))))
            .expect("mine");
        assert!(d.events.is_empty());
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(text.contains(&child.to_string()));

        c.apply(&CapEvent::Workflow(Event::Reported { child }));
        let d = c
            .handle(&Msg::Tool(&call(STATUS_TOOL, json!({}))))
            .expect("mine");
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(!text.contains(&child.to_string()));
    }

    /// Both tools, advertised through `tools()` — which is what routes the call
    /// to the mailbox, where the intent can be journaled and the call parked.
    #[test]
    fn it_advertises_both_tools() {
        assert_eq!(
            WorkflowCapability::default()
                .tools()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>(),
            vec![INVOKE_TOOL, STATUS_TOOL]
        );
    }

    /// `outstanding` is what invariant 6 and every delivery are decided from,
    /// so losing it in the journal loses the agent.
    #[test]
    fn the_runs_in_flight_survive_a_slice_round_trip() {
        let mut c = WorkflowCapability::default();
        let child = invoked(&mut c);
        let caps = Capabilities::new(vec![Box::new(c)]);

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::Workflow(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            back.outstanding.into_iter().collect::<Vec<_>>(),
            vec![child],
            "the reload was rebuilt from config and lost the run in flight"
        );
    }

    /// Everything else falls through, so the offer reaches whoever does own it.
    #[test]
    fn another_message_is_not_mine() {
        let c = WorkflowCapability::default();
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
