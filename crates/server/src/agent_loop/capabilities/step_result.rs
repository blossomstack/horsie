//! `submit_result`: how a workflow step says it is done, and what it returns.
//!
//! The declared outcomes and fields are held here rather than looked up from
//! the graph at call time, because the tool's input schema and the check its
//! arguments are held to must be one declaration — a step that advertises
//! `outcome: p0 | p2` and then validates against something else is the drift
//! this shape removes.
//!
//! # Concluding is not parking
//!
//! Both stop the run, which is exactly why the old code could treat `ask_user`
//! and `submit_result` alike and sort them out afterwards by matching tool names
//! in the actor. They are not alike. A park owes a result later — the dangling
//! `tool_use` *is* the parked agent, and an answer arrives against it. A
//! conclusion owes nothing ever, and carries an output a park has nowhere to
//! put. So this capability answers [`Act::Conclude`], and the difference lives
//! in the act rather than in a name match downstream of it.
//!
//! Concluding is still not *routing*. Which step runs next, what its input is,
//! and whether the run is over are the workflow runner's decisions, made from
//! the journaled output. This capability says "the step is finished, and here is
//! its result"; nothing here can end a run, so an outcome cannot be acted on
//! twice by two different owners.

use super::{Act, CapCommand, Decision, Mailbox, Msg, SetupError};
use crate::agent_loop::state::AgentDomainEvent;
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use crate::sessions::workflow::{SUBMIT_RESULT_TOOL, result_schema, validate_result};
use horsie_agentcore::{ToolSpec, Toolbox};
use horsie_models::workflow::{StepField, StepOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// What the model asked this capability to do.
pub enum Command {
    /// `submit_result`, with the result still unvalidated: an undeclared
    /// outcome is a refusal the step has to see.
    Submit { input: Value },
}

/// Appended to a workflow step's system prompt: what a step is, how it ends,
/// and that its result is what decides where the run goes next. Deliberately
/// short — `submit_result` carries its own schema.
///
/// The paragraph about ending a turn earns its length. A step ends when it
/// calls `submit_result`, but a turn may legitimately end without one — parked
/// on a question, on a timer, or waiting for subagents — and a model that does
/// not know the difference either submits early to be safe or stops with
/// nothing to wake it.
const STEP_PROMPT_SUFFIX: &str = "# Workflow step\n\
You are one step of a workflow, not a conversation. Your instruction and the previous \
step's result are in the message above. You share one workspace with every other step: \
what you change on disk is what the next step sees. You may spawn subagents with \
spawn_agent. You cannot rename the session.\n\n\
Finish by calling `submit_result`. What you submit is this step's result *and* what the \
workflow reads to decide which step runs next, so make it accurate and self-contained. \
Ending a turn without it is only safe while something will wake you — a question you \
asked, a timer you armed, or a subagent still running. If nothing will, and the work is \
done, submit.";

/// What the model is told the tool is for. Its schema is per step; this
/// sentence is not.
const TOOL_DESCRIPTION: &str = "Finish this step: deliver its result. Call this once the step's \
     work is done — it ends the step, and what you submit decides which step runs next.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepResultCapability {
    /// The values this step's `outcome` may take.
    pub outcomes: Vec<StepOutcome>,
    /// What the result carries beyond `outcome` and `description`.
    pub fields: Vec<StepField>,
    /// Whether this step may stop and ask the person.
    pub interactive: bool,
}

/// What this capability records: the step delivered its result.
impl StepResultCapability {
    #[must_use]
    pub fn new(outcomes: Vec<StepOutcome>, fields: Vec<StepField>, interactive: bool) -> Self {
        Self {
            outcomes,
            fields,
            interactive,
        }
    }

    /// The model submitted this step's result.
    fn submitted(&self, call: &str, input: &Value) -> Decision {
        match validate_result(input, &self.outcomes, &self.fields) {
            Ok(()) => Decision::record(vec![AgentDomainEvent::StepResultSubmitted {
                output: input.clone(),
            }])
            .then(Act::Conclude {
                output: input.clone(),
            }),
            // Journals nothing: an undeclared outcome in the log is one the
            // workflow runner would try to route on. The validator's own words
            // go back, against the call the model actually made, so it can
            // correct the field it got wrong.
            Err(reason) => Decision::refuse(call, format!("submit_result was rejected: {reason}")),
        }
    }
}

impl StepResultCapability {
    /// The step's own result tool, built from the schema it declared.
    ///
    /// This is the whole of what makes one step's equipment differ from the
    /// next step's, and why equipment is computed per agent rather than per
    /// runner: the workflow runner outlives every step, so a single per-runner
    /// spec could only ever describe one of them.
    ///
    /// [`Self::interactive`] is not read here. Whether a step may stop and ask
    /// is answered by whether it was equipped with
    /// [`super::ask_user::AskUserCapability`] — one capability owns `ask_user`,
    /// its tool and the answer that comes back, and a second advertisement of
    /// the same tool from over here would offer a question nothing could route
    /// the answer to.
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![ClaimedTool::new(
            ToolSpec {
                name: SUBMIT_RESULT_TOOL.to_string(),
                description: TOOL_DESCRIPTION.to_string(),
                input_schema: result_schema(&self.outcomes, &self.fields),
            },
            |input, to| CapCommand::StepResult(Command::Submit { input }, to),
        )]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl StepResultCapability {
    pub fn name(&self) -> &'static str {
        "step_result"
    }

    /// The paragraph that says what a step is, and nothing else.
    ///
    /// The tool itself is claimed by this capability's own
    /// [`super::Capability::layer`] rather than pushed as a layer here, which is the
    /// change the move made: a layer pushed here runs on the agent's task,
    /// where there is no mailbox to journal the submitted output on and nothing
    /// that could conclude the step.
    pub async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        spec.say("step_result", STEP_PROMPT_SUFFIX);
        Ok(())
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(), mailbox)
    }

    pub fn command(&self, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::StepResult(cmd, to) = cmd else {
            return None;
        };
        let Command::Submit { input } = cmd;
        Some(self.submitted(&to.call, input))
    }

    /// Nothing here is this one's, and nothing is re-asked on a load: this
    /// capability asks the session for nothing, so it holds nothing a dead
    /// process could have failed to send.
    pub fn handle(&self, _msg: &Msg) -> Option<Decision> {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::testing::{
        advertised, advertised_by, answering, equipped, facts, loading, settings, someone_elses,
        spec, specs_of,
    };
    use crate::agent_loop::capabilities::{Capabilities, Capability, TurnEvent};
    use horsie_models::workflow::StepFieldType;
    use serde_json::{Value, json};

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

    fn submitted(c: &StepResultCapability, id: &str, input: Value) -> Decision {
        c.command(&CapCommand::StepResult(
            Command::Submit { input },
            answering(id),
        ))
        .expect("mine")
    }

    /// The advertised schema, as the model is shown it.
    fn schema(c: &StepResultCapability) -> Value {
        specs_of(&Capability::StepResult(c.clone()), &facts())
            .into_iter()
            .find(|s| s.name == SUBMIT_RESULT_TOOL)
            .expect("a step is equipped to submit")
            .input_schema
    }

    /// **A valid submission concludes, carrying the submitted JSON verbatim.**
    /// Not a park: a park owes a result later and has nowhere to put an output,
    /// and a step parked on its own result would wait for an answer nobody is
    /// ever going to send. The same JSON is journaled, because the workflow
    /// runner routes on what the log says rather than on what the act carried.
    #[test]
    fn a_valid_submission_concludes_with_the_submitted_output() {
        let output = json!({
            "outcome": "p0",
            "description": "found the flake",
            "owner": "shawn",
        });
        let d = submitted(&cap(false), "call-1", output.clone());

        let [AgentDomainEvent::StepResultSubmitted { output: journaled }] = d.events.as_slice()
        else {
            panic!("expected one Submitted, got {:?}", d.events)
        };
        assert_eq!(journaled, &output, "the journal must carry it verbatim");

        let [Act::Conclude { output: concluded }] = d.acts.as_slice() else {
            panic!("a submission that did not conclude: {:?}", d.acts)
        };
        assert_eq!(concluded, &output, "the act must carry it verbatim");
    }

    /// **An undeclared outcome is refused and journals nothing.** Journaling it
    /// would hand the workflow runner a value it has no transition for, which is
    /// exactly what the validator exists to keep out of the log — and concluding
    /// on it would end the step as though the graph were finished.
    #[test]
    fn an_undeclared_outcome_is_refused_and_journals_nothing() {
        let d = submitted(
            &cap(false),
            "call-1",
            json!({"outcome": "p9", "description": "x", "owner": "shawn"}),
        );

        assert!(d.events.is_empty(), "nothing undeclared reaches the log");
        // `Refuse`, not `Answer`: this reaches the model as a tool *error*.
        // `is_error` is read by agentcore's loop detector and the nudge budget,
        // and a step resubmitting the same bad outcome is exactly where it
        // shows.
        let [Act::Refuse { call, reason: text }] = d.acts.as_slice() else {
            panic!("expected one Refuse, got {:?}", d.acts)
        };
        // Against the model's own call id. A literal here would answer the wrong
        // call: a turn runs its tool calls concurrently, and the id is what an
        // answer correlates by.
        assert_eq!(call, "call-1");
        assert!(
            text.contains("p9") && text.contains("p0"),
            "the validator's own words go back so the model can correct itself: {text}"
        );
    }

    /// A missing required field is refused the same way — the check and the
    /// schema are one declaration, so anything the schema demands is something
    /// the refusal can name.
    #[test]
    fn a_missing_required_field_is_refused() {
        let d = submitted(
            &cap(false),
            "call-1",
            json!({"outcome": "p0", "description": "did it"}),
        );
        assert!(d.events.is_empty());
        let [Act::Refuse { reason: text, .. }] = d.acts.as_slice() else {
            panic!("expected one Refuse, got {:?}", d.acts)
        };
        assert!(text.contains("'owner' is required"), "{text}");
    }

    /// **The advertised schema names this step's own outcomes, not the
    /// defaults.** The tool the model is shown and the check its arguments are
    /// held to are one declaration, so a step cannot advertise `p0 | p2` and
    /// validate something else.
    #[test]
    fn the_result_tool_advertises_this_steps_own_outcomes() {
        let schema = schema(&cap(false));
        assert_eq!(schema["properties"]["outcome"]["enum"], json!(["p0", "p2"]));

        let rendered = schema.to_string();
        assert!(
            !rendered.contains("success") && !rendered.contains("failure"),
            "the step was advertised with the default outcomes: {rendered}"
        );
        // Each value's meaning travels with it, and the declared field keeps its
        // own type and its place in `required`.
        let doc = schema["properties"]["outcome"]["description"]
            .as_str()
            .expect("the enum alone says what may be said, not what any of it means");
        assert!(doc.contains("p0: drop everything"), "{doc}");
        assert!(doc.contains("p2: file it"), "{doc}");
        assert_eq!(schema["properties"]["owner"]["type"], "string");
        assert!(
            schema["required"]
                .as_array()
                .expect("a list")
                .contains(&json!("owner"))
        );
    }

    /// A list-typed field, which the string case above cannot tell apart from a
    /// schema that types everything as a string.
    #[test]
    fn a_declared_list_field_is_advertised_as_an_array_of_strings() {
        let c = StepResultCapability::new(
            outcomes(),
            vec![StepField {
                name: "files".into(),
                kind: StepFieldType::StringList,
                description: "what changed".into(),
                required: Some(true),
            }],
            false,
        );
        let schema = schema(&c);
        assert_eq!(schema["properties"]["files"]["type"], "array");
        assert_eq!(schema["properties"]["files"]["items"]["type"], "string");
    }

    /// Setup is left with the paragraph and nothing else: the tool is claimed
    /// by this capability's own layer, which is what routes the call to this
    /// actor's mailbox, where the output can be journaled.
    #[tokio::test]
    async fn the_step_says_what_it_is_and_equips_no_layer() {
        let mut spec = spec();
        cap(false)
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");
        assert!(spec.prompt.iter().any(|s| s.key == "step_result"));
        assert_eq!(
            equipped(spec),
            Vec::<String>::new(),
            "the tool is dispatched through the mailbox, not through a layer"
        );
    }

    /// An interactive step gets `ask_user` alongside its result tool — from the
    /// capability that owns that tool, which it is equipped with. Held here
    /// because it is the property a step's equipment is judged on, and the two
    /// capabilities together are what makes it true.
    #[tokio::test]
    async fn an_interactive_step_is_equipped_to_ask() {
        let caps = Capabilities::new(vec![
            Capability::AskUser(super::super::ask_user::AskUserCapability::new()),
            Capability::StepResult(cap(true)),
        ]);
        let (_spec, degraded) = caps
            .equip(&loading(), settings())
            .await
            .expect("nothing fatal");
        assert!(degraded.is_empty());

        let names = advertised(&caps, &facts());
        assert!(names.contains(&super::super::ask_user::TOOL.to_string()));
        assert!(names.contains(&SUBMIT_RESULT_TOOL.to_string()));
    }

    /// The declaration a step is built from survives the journal. A reload that
    /// lost it would advertise the default outcomes and validate against nothing
    /// the author wrote.
    #[test]
    fn a_steps_declaration_survives_a_slice_round_trip() {
        let caps = Capabilities::new(vec![Capability::StepResult(cap(true))]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let [Capability::StepResult(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            back.outcomes
                .iter()
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>(),
            vec!["p0", "p2"]
        );
        assert_eq!(back.fields.len(), 1);
        assert!(back.interactive);
    }

    #[test]
    fn another_capabilitys_command_is_not_mine() {
        let c = cap(false);
        assert!(c.command(&someone_elses()).is_none());
        // And with no park to hold, a turn boundary is nothing to it either.
        assert!(c.handle(&Msg::Turn(TurnEvent::Ended)).is_none());
    }

    /// It claims the one tool it answers for, so a step's result is reached by
    /// the layer that builds its command rather than by a name matched later.
    #[test]
    fn it_advertises_its_own_result_tool() {
        assert_eq!(
            advertised_by(&Capability::StepResult(cap(false)), &facts()),
            vec![SUBMIT_RESULT_TOOL]
        );
    }
}
