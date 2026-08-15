//! `invoke_workflow` and `workflow_status`: an agent starting a run, and
//! reading back the ones still going.
//!
//! The capability the redesign exists for. A run invoked this way is a runner
//! inside the same session, parented on the agent that asked, so a session may
//! hold any number of them at once — the shape this replaces had one
//! `Option<WorkflowRunState>` on the session and inferred a subagent's owning
//! tree from "which step is in flight", an inference with no answer once two
//! runs are live.
//!
//! Shaped exactly like [`super::sub_agent`], and for the same reason: a run
//! that reports owes its output to one agent, `outstanding` is the one fact
//! saying whether it has been told and whom to tell, and delivery tells before
//! it acknowledges, so a crash between the two replays as a report still owed.
//!
//! The two capabilities do not overlap. A [`ChildOutcome::SubAgent`] is
//! declined here even for a child id this capability holds: the outcome's kind
//! and the owning capability have to agree, and `None` is how they say so.

use super::{CapEvent, Decision, Handler};
use crate::sessions::runners::action::{Action, AgentSpec, PromptSection, RunnerArgs, ToolLayer};
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::message::{
    Caller, ChildMsg, ChildOutcome, Message, ToolCall, WorkflowOutcome,
};
use crate::sessions::workflow::WorkflowRunSpec;
use horsie_models::agent::SubAgentResultPart;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The tool that starts a run.
pub const INVOKE_TOOL: &str = "invoke_workflow";
/// The tool that reads back what is still running.
pub const STATUS_TOOL: &str = "workflow_status";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowCapability {
    /// Which run, and which of my agents invoked it.
    pub outstanding: BTreeMap<RunnerId, AgentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Started { child: RunnerId, from: AgentId },
    Reported { child: RunnerId },
}

/// The tool's arguments.
///
/// The model names a workflow; it never writes a graph. Turning that name into
/// a definition is a database read, and a database read may not happen on the
/// session mailbox — so the layer that owns this tool resolves it off-mailbox
/// and attaches the result to the call it forwards. That is why `graph` is
/// here and optional while the model's schema has only the two fields above
/// it: the name is what was asked for, and the resolved graph reaches
/// [`RunnerArgs::Workflow`] one hop later. A call that arrives without one is a
/// name that resolved to nothing, and is refused rather than run.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub workflow: String,
    pub input: String,
    #[serde(default)]
    pub graph: Option<Arc<WorkflowRunSpec>>,
}

impl WorkflowCapability {
    fn on_tool(&self, caller: Caller, t: &ToolCall) -> Option<Decision> {
        match t.name.as_str() {
            INVOKE_TOOL => {
                let req: Request = serde_json::from_value(t.input.clone()).ok()?;
                let Some(graph) = req.graph else {
                    return Some((
                        Vec::new(),
                        vec![Action::Reply {
                            text: format!("no workflow named `{}`", req.workflow),
                        }],
                    ));
                };
                // Minted here, not in `apply`: a decision may be
                // non-deterministic, a fold may not. The event and the action
                // then name the same run, and replay lands the id the log has.
                let child = RunnerId::new_v4();
                Some((
                    vec![CapEvent::Workflow(Event::Started {
                        child,
                        from: caller.agent,
                    })],
                    vec![Action::CreateChild {
                        id: child,
                        kind: RunnerKind::Workflow,
                        args: RunnerArgs::Workflow {
                            graph,
                            input: req.input,
                        },
                        parent: caller.agent,
                    }],
                ))
            }
            STATUS_TOOL => Some((
                Vec::new(),
                vec![Action::Reply {
                    text: self.render_status(),
                }],
            )),
            _ => None,
        }
    }

    fn on_child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(o),
            } => {
                // Not one of mine: `None` rather than a delivery to an agent
                // that never invoked anything.
                let to = *self.outstanding.get(child)?;
                Some(self.deliver(*child, to, part(*child, o)))
            }
            // A worker's report is the subagent capability's, even when this
            // capability holds that child id. Two capabilities must never both
            // plausibly claim one outcome.
            ChildMsg::Outcome {
                outcome: ChildOutcome::SubAgent(_),
                ..
            } => None,
            ChildMsg::Failed { child, error } => {
                let to = *self.outstanding.get(child)?;
                Some(self.deliver(
                    *child,
                    to,
                    failed_part(*child, child.to_string(), error.clone()),
                ))
            }
            // A run is runnable the moment it is created; only a fork has a
            // seed that can land later.
            ChildMsg::Ready { .. } => None,
        }
    }

    fn deliver(&self, child: RunnerId, to: AgentId, part: SubAgentResultPart) -> Decision {
        (
            vec![CapEvent::Workflow(Event::Reported { child })],
            vec![Action::Deliver {
                to,
                from: child,
                part: Box::new(part),
            }],
        )
    }

    /// Only what is still running: a run that reported has already been
    /// delivered into the invoking agent's transcript.
    fn render_status(&self) -> String {
        if self.outstanding.is_empty() {
            return "No workflow runs are in flight.".to_string();
        }
        let mut text = format!("{} workflow run(s) in flight:", self.outstanding.len());
        for (child, from) in &self.outstanding {
            text.push_str(&format!("\n- {child} (invoked by {from})"));
        }
        text
    }
}

/// A finished run's terminal output, in the shape the invoking agent's inbox
/// takes — the same part a subagent's report arrives as, because to the agent
/// that asked they are the same thing: work it delegated, come back.
///
/// The timestamps are zero because this capability holds neither; they live on
/// the run's `RunnerRecord`, and a client shows no duration rather than one
/// that never happened.
fn part(child: RunnerId, outcome: &WorkflowOutcome) -> SubAgentResultPart {
    match outcome {
        WorkflowOutcome::Finished { output } => SubAgentResultPart {
            subagent_id: child.to_string(),
            label: "workflow".to_string(),
            status: "completed".to_string(),
            text: render(output),
            spawned_at_ms: 0,
            ended_at_ms: 0,
        },
        WorkflowOutcome::Failed { error } => {
            failed_part(child, "workflow".to_string(), error.clone())
        }
    }
}

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

/// A run's output is a `Value`, and a step's output is usually already text.
/// Rendering a JSON string as `"…"` would hand the model its own quotes back.
fn render(output: &Value) -> String {
    if let Value::String(s) = output {
        return s.clone();
    }
    serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
}

impl Handler for WorkflowCapability {
    fn setup(&self, spec: &mut AgentSpec) {
        spec.layers.push(ToolLayer::InvokeWorkflow);
        spec.prompt.push(PromptSection {
            key: "workflow",
            body: "Run a defined workflow with `invoke_workflow` when the job \
                   is one you already have a definition for. The run happens \
                   in this session and shares its workspace; its terminal \
                   output is delivered to you automatically when it finishes, \
                   so carry on meanwhile and use `workflow_status` only when \
                   asked for progress — never as a poll."
                .to_string(),
        });
    }

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        match msg {
            Message::Tool(t) => self.on_tool(caller, t),
            Message::Child(m) => self.on_child(m),
            Message::Command(_) | Message::Ask(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        let CapEvent::Workflow(e) = event else { return };
        match e {
            Event::Started { child, from } => {
                self.outstanding.insert(*child, *from);
            }
            Event::Reported { child } => {
                self.outstanding.remove(child);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::runners::message::AskMsg;

    fn graph() -> Arc<WorkflowRunSpec> {
        Arc::new(WorkflowRunSpec {
            workflow: "release".into(),
            start: "first".into(),
            steps: vec![],
            input: String::new(),
            max_steps: 8,
        })
    }

    fn invocation() -> serde_json::Value {
        serde_json::json!({
            "workflow": "release",
            "input": "cut 1.2.0",
            "graph": serde_json::to_value(&*graph()).unwrap(),
        })
    }

    fn invoke(c: &mut WorkflowCapability, caller: Caller) -> RunnerId {
        let (events, actions) = c
            .handle(caller, &tool(INVOKE_TOOL, invocation()))
            .expect("mine");
        c.apply(&events[0]);
        let Action::CreateChild { id, .. } = &actions[0] else {
            panic!("expected a create, got {:?}", actions[0]);
        };
        *id
    }

    /// The event and the action must name the same run, or the log records a
    /// run nothing created and the agent waits for ever. The graph rides on
    /// the args, which is what makes an ad-hoc one expressible later.
    #[test]
    fn an_invocation_journals_and_creates_the_same_run() {
        let c = WorkflowCapability::default();
        let caller = caller();
        let (events, actions) = c
            .handle(caller, &tool(INVOKE_TOOL, invocation()))
            .expect("mine");
        let CapEvent::Workflow(Event::Started { child, from }) = &events[0] else {
            panic!("expected a start, got {:?}", events[0]);
        };
        assert_eq!(*from, caller.agent);
        let Action::CreateChild {
            id,
            kind,
            args,
            parent,
        } = &actions[0]
        else {
            panic!("expected a create, got {:?}", actions[0]);
        };
        assert_eq!(id, child);
        assert_eq!(*kind, RunnerKind::Workflow);
        assert_eq!(*parent, caller.agent);
        let RunnerArgs::Workflow { graph, input } = args else {
            panic!("expected workflow args, got {args:?}");
        };
        assert_eq!(graph.workflow, "release");
        assert_eq!(input, "cut 1.2.0");
    }

    /// A name that resolved to nothing is refused in words, not journaled: a
    /// `Started` for a run that cannot be created would leave the agent owed a
    /// report nothing will ever send.
    #[test]
    fn an_unresolved_name_is_refused_and_journals_nothing() {
        let c = WorkflowCapability::default();
        let (events, actions) = c
            .handle(
                caller(),
                &tool(
                    INVOKE_TOOL,
                    serde_json::json!({"workflow": "nope", "input": "x"}),
                ),
            )
            .expect("mine");
        assert!(events.is_empty());
        let Action::Reply { text } = &actions[0] else {
            panic!("expected a reply, got {:?}", actions[0]);
        };
        assert!(text.contains("nope"));
    }

    /// `outstanding` says both "an output is owed" and "to whom".
    #[test]
    fn started_records_the_run_and_reported_clears_it() {
        let mut c = WorkflowCapability::default();
        let caller = caller();
        let child = invoke(&mut c, caller);
        assert_eq!(c.outstanding.get(&child), Some(&caller.agent));

        c.apply(&CapEvent::Workflow(Event::Reported { child }));
        assert!(c.outstanding.is_empty());
    }

    /// The run's terminal output reaches the agent that invoked it, rendered
    /// as text rather than as a quoted JSON string.
    #[test]
    fn a_finished_run_delivers_its_output_to_the_invoker() {
        let mut c = WorkflowCapability::default();
        let asker = caller();
        let child = invoke(&mut c, asker);
        let (events, actions) = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                        output: serde_json::json!("shipped"),
                    }),
                }),
            )
            .expect("mine");
        assert!(matches!(
            events[0],
            CapEvent::Workflow(Event::Reported { .. })
        ));
        let Action::Deliver { to, from, part } = &actions[0] else {
            panic!("expected a delivery, got {:?}", actions[0]);
        };
        assert_eq!(*to, asker.agent);
        assert_eq!(*from, child);
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "shipped");
    }

    /// A run that failed is still an answer. An agent blocked on one that died
    /// and was never told would wait for ever.
    #[test]
    fn a_failed_run_is_delivered_as_a_failed_part() {
        let mut c = WorkflowCapability::default();
        let child = invoke(&mut c, caller());
        let (_, actions) = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Failed {
                        error: "step 2 failed".into(),
                    }),
                }),
            )
            .expect("mine");
        let Action::Deliver { part, .. } = &actions[0] else {
            panic!("expected a delivery, got {:?}", actions[0]);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "step 2 failed");
    }

    /// A run that never started takes the same delivery path.
    #[test]
    fn a_run_that_never_started_is_reported_as_failed() {
        let mut c = WorkflowCapability::default();
        let asker = caller();
        let child = invoke(&mut c, asker);
        let (_, actions) = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Failed {
                    child,
                    error: "the create failed".into(),
                }),
            )
            .expect("mine");
        let Action::Deliver { to, part, .. } = &actions[0] else {
            panic!("expected a delivery, got {:?}", actions[0]);
        };
        assert_eq!(*to, asker.agent);
        assert_eq!(part.status, "failed");
    }

    /// The outcome's kind and the owning capability must agree. A subagent's
    /// report is declined *even for a child id this capability holds* — which
    /// is the only way two capabilities on one runner cannot both claim it.
    #[test]
    fn a_subagent_outcome_is_never_mine_even_for_a_child_i_hold() {
        let mut c = WorkflowCapability::default();
        let child = invoke(&mut c, caller());
        assert!(
            c.handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::SubAgent(
                        crate::sessions::runners::message::SubAgentOutcome::Completed {
                            label: "l".into(),
                            report: "r".into(),
                        }
                    ),
                }),
            )
            .is_none()
        );
    }

    /// A run this capability did not invoke is not its business.
    #[test]
    fn an_outcome_for_a_run_i_did_not_invoke_is_not_mine() {
        let c = WorkflowCapability::default();
        assert!(
            c.handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child: RunnerId::new_v4(),
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                        output: serde_json::json!("done"),
                    }),
                }),
            )
            .is_none()
        );
    }

    /// A status read journals nothing, and lists only what is still going.
    #[test]
    fn status_lists_the_runs_in_flight_and_journals_nothing() {
        let mut c = WorkflowCapability::default();
        let caller = caller();
        let child = invoke(&mut c, caller);
        let (events, actions) = c
            .handle(caller, &tool(STATUS_TOOL, serde_json::json!({})))
            .expect("mine");
        assert!(events.is_empty());
        let Action::Reply { text } = &actions[0] else {
            panic!("expected a reply, got {:?}", actions[0]);
        };
        assert!(text.contains(&child.to_string()));

        c.apply(&CapEvent::Workflow(Event::Reported { child }));
        let (_, actions) = c
            .handle(caller, &tool(STATUS_TOOL, serde_json::json!({})))
            .expect("mine");
        let Action::Reply { text } = &actions[0] else {
            panic!("expected a reply, got {:?}", actions[0]);
        };
        assert!(!text.contains(&child.to_string()));
    }

    #[test]
    fn setup_equips_the_invoke_layer() {
        let mut spec = AgentSpec::default();
        WorkflowCapability::default().setup(&mut spec);
        assert!(spec.has(&ToolLayer::InvokeWorkflow));
    }

    /// Everything else falls through, so the offer reaches whoever does own it.
    #[test]
    fn another_message_is_not_mine() {
        let c = WorkflowCapability::default();
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
        assert!(
            c.handle(
                caller(),
                &Message::Ask(AskMsg::Answered { answers: vec![] })
            )
            .is_none()
        );
    }
}
