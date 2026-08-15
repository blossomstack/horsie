//! `submit_result`: how a workflow step says it is done, and what it returns.
//!
//! The declared outcomes and fields are held here rather than looked up from
//! the graph at call time, because the tool's input schema and the check its
//! arguments are held to must be one declaration — a step that advertises
//! `outcome: p0 | p2` and then validates against something else is the drift
//! this shape removes.
//!
//! Taking a valid submission journals it and asks the session for *nothing*.
//! Concluding the step — routing on the outcome, composing the next step's
//! input, ending the run — is the workflow runner's decision, made from the
//! journaled output. That separation is the point: this capability cannot end a
//! run, so an outcome cannot be acted on twice by two different owners.

use super::{CapEvent, Decision, Handler};
use crate::sessions::runners::action::{Action, AgentSpec, ToolLayer};
use crate::sessions::runners::message::{Caller, Message};
use crate::sessions::workflow::{SUBMIT_RESULT_TOOL, validate_result};
use horsie_models::workflow::{StepField, StepOutcome};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResultCapability {
    /// The values this step's `outcome` may take.
    pub outcomes: Vec<StepOutcome>,
    /// What the result carries beyond `outcome` and `description`.
    pub fields: Vec<StepField>,
    /// Whether this step may stop and ask the person.
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Submitted { output: serde_json::Value },
}

impl StepResultCapability {
    #[must_use]
    pub fn new(outcomes: Vec<StepOutcome>, fields: Vec<StepField>, interactive: bool) -> Self {
        Self {
            outcomes,
            fields,
            interactive,
        }
    }
}

impl Handler for StepResultCapability {
    fn setup(&self, spec: &mut AgentSpec) {
        spec.layers.push(ToolLayer::SubmitResult {
            outcomes: self.outcomes.clone(),
            fields: self.fields.clone(),
        });
        // These two lines are the whole of what makes one step's equipment
        // differ from the next step's: its own result schema, and `ask_user`
        // only if it declared itself interactive. It is also why equipment is
        // computed per agent rather than per runner — the workflow runner
        // outlives every step, so a single per-runner spec could only describe
        // one of them.
        if self.interactive {
            spec.layers.push(ToolLayer::AskUser);
        }
    }

    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        if t.name != SUBMIT_RESULT_TOOL {
            return None;
        }
        match validate_result(&t.input, &self.outcomes, &self.fields) {
            Ok(()) => Some((
                vec![CapEvent::StepResult(Event::Submitted {
                    output: t.input.clone(),
                })],
                vec![],
            )),
            // Journal nothing: an undeclared outcome in the log is one the
            // runner would try to route on. The validator's own words go back
            // so the model can correct the field it actually got wrong.
            Err(reason) => Some((
                vec![],
                vec![Action::Reply {
                    text: format!("submit_result was rejected: {reason}"),
                }],
            )),
        }
    }

    fn apply(&mut self, _event: &CapEvent) {
        // Nothing to fold: the submitted output belongs to the step's own
        // record, which the workflow runner keeps. Holding a copy here would
        // be a second answer to "what did step 3 return".
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use horsie_models::workflow::StepFieldType;

    fn outcomes() -> Vec<StepOutcome> {
        vec![
            StepOutcome {
                value: "p0".into(),
                description: "drop everything".into(),
            },
            StepOutcome {
                value: "p2".into(),
                description: "file it".into(),
            },
        ]
    }

    fn fields() -> Vec<StepField> {
        vec![StepField {
            name: "owner".into(),
            kind: StepFieldType::String,
            description: "who takes it".into(),
            required: Some(true),
        }]
    }

    fn cap(interactive: bool) -> StepResultCapability {
        StepResultCapability::new(outcomes(), fields(), interactive)
    }

    /// A valid submission is journaled and asks for nothing. If an action ever
    /// appears here, the capability has started concluding the step, and the
    /// runner's routing is no longer the only thing that ends a step.
    #[test]
    fn a_valid_submission_journals_the_output_and_acts_on_nothing() {
        let submitted = serde_json::json!({
            "outcome": "p0",
            "description": "found the flake",
            "owner": "shawn",
        });
        let (events, actions) = cap(false)
            .handle(caller(), &tool(SUBMIT_RESULT_TOOL, submitted.clone()))
            .expect("mine");
        assert!(actions.is_empty());
        let [CapEvent::StepResult(Event::Submitted { output })] = events.as_slice() else {
            panic!("expected one Submitted, got {events:?}")
        };
        assert_eq!(output, &submitted);
    }

    /// An undeclared outcome replies and journals nothing. Journaling it would
    /// hand the runner a value it has no transition for, which is exactly what
    /// the validator exists to keep out of the log.
    #[test]
    fn an_invalid_submission_replies_and_journals_nothing() {
        let (events, actions) = cap(false)
            .handle(
                caller(),
                &tool(
                    SUBMIT_RESULT_TOOL,
                    serde_json::json!({"outcome": "p9", "description": "x", "owner": "shawn"}),
                ),
            )
            .expect("mine");
        assert!(events.is_empty(), "nothing undeclared reaches the log");
        let [Action::Reply { text }] = actions.as_slice() else {
            panic!("expected one Reply, got {actions:?}")
        };
        assert!(
            text.contains("p9") && text.contains("p0"),
            "the validator's own words go back so the model can correct itself: {text}"
        );
    }

    /// An interactive step gets `ask_user` alongside its result tool.
    #[test]
    fn an_interactive_step_is_equipped_to_ask() {
        let mut spec = AgentSpec::default();
        cap(true).setup(&mut spec);
        assert!(spec.has(&ToolLayer::AskUser));
    }

    /// And a non-interactive one is not — the same capability, one flag apart,
    /// which is what lets step 1 stop for a person and step 2 not.
    #[test]
    fn a_non_interactive_step_gets_the_result_tool_and_no_ask_user() {
        let mut spec = AgentSpec::default();
        cap(false).setup(&mut spec);
        assert!(spec.has(&ToolLayer::SubmitResult {
            outcomes: outcomes(),
            fields: fields(),
        }));
        assert!(!spec.has(&ToolLayer::AskUser));
    }

    #[test]
    fn another_tool_is_not_mine() {
        assert!(
            cap(false)
                .handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }
}
