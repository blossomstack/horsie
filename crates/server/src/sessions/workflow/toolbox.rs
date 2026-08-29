//! The tool a workflow step finishes with.
//!
//! An interactive agent ends its turn with plain text and asks through
//! `ask_user`. A step does neither to *finish*: it calls `submit_result`, whose
//! input schema is compiled from the step's declared outcomes and fields, which
//! is what lets a transition match on `outcome` at all.
//!
//! Calling it ends the run — the tool answers [`ToolOutcome::StopRun`] — and no
//! result is recorded, so the call stays dangling exactly as a park does. The
//! step is over; nothing will ever answer it.
//!
//! Validation happens here, on the way in. A rejected payload comes back as an
//! ordinary [`ToolCallError::InvalidInput`], which the model sees as a tool
//! result and re-issues against, bounded by the loop's retry budget. That is
//! the only thing standing between an outcome the step never declared and a
//! driver trying to route on it.

use crate::sessions::workflow::result_schema::{
    SUBMIT_RESULT_TOOL, result_schema, validate_result,
};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use horsie_models::workflow::{StepField, StepOutcome};
use serde_json::Value;
use std::sync::Arc;

/// Wraps a step's toolbox, adding `submit_result`.
pub struct StepResultToolbox {
    inner: Arc<dyn Toolbox>,
    spec: ToolSpec,
    outcomes: Vec<StepOutcome>,
    fields: Vec<StepField>,
}

impl StepResultToolbox {
    pub fn wrap(
        inner: Arc<dyn Toolbox>,
        outcomes: Vec<StepOutcome>,
        fields: Vec<StepField>,
    ) -> Arc<dyn Toolbox> {
        let spec = ToolSpec {
            name: SUBMIT_RESULT_TOOL.to_string(),
            description: "Finish this step: deliver its result. Call this once the step's work \
                 is done — it ends the step, and what you submit decides which step runs next."
                .to_string(),
            input_schema: result_schema(&outcomes, &fields),
        };
        Arc::new(Self {
            inner,
            spec,
            outcomes,
            fields,
        })
    }
}

#[async_trait]
impl Toolbox for StepResultToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(self.spec.clone());
        specs
    }

    /// Forwards `tool_call_id` untouched. It used to pass a literal `"tc1"`,
    /// which is not cosmetic: the id is the runtime's correlation key
    /// (`ToolCallRequest.call_id`) and the key of the in-flight set a cancel
    /// walks. The agent loop runs a turn's tool calls concurrently, so two
    /// parallel calls from one step shared an id — their replies could correlate
    /// to the wrong call, and a cancel reached only one of them.
    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name == SUBMIT_RESULT_TOOL {
            return match validate_result(&input, &self.outcomes, &self.fields) {
                Ok(()) => Ok(ToolOutcome::StopRun),
                Err(reason) => Err(ToolCallError::InvalidInput(reason)),
            };
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_agentcore::ToolboxImpl;
    use horsie_models::workflow::StepFieldType;

    fn outcomes() -> Vec<StepOutcome> {
        vec![StepOutcome {
            value: "success".into(),
            description: "done".into(),
        }]
    }

    fn base() -> Arc<dyn Toolbox> {
        Arc::new(ToolboxImpl::new())
    }

    fn submitted() -> Value {
        serde_json::json!({"outcome": "success", "description": "did it"})
    }

    #[test]
    fn a_step_advertises_submit_result_with_its_declared_schema() {
        let fields = vec![StepField {
            name: "files".into(),
            kind: StepFieldType::StringList,
            description: "what changed".into(),
            required: Some(true),
        }];
        let tb = StepResultToolbox::wrap(base(), outcomes(), fields);
        let spec = tb
            .specs()
            .into_iter()
            .find(|s| s.name == SUBMIT_RESULT_TOOL)
            .expect("the step's terminal tool is offered");
        assert_eq!(
            spec.input_schema["properties"]["outcome"]["enum"],
            serde_json::json!(["success"])
        );
        assert_eq!(spec.input_schema["properties"]["files"]["type"], "array");
    }

    #[tokio::test]
    async fn submitting_stops_the_run() {
        let tb = StepResultToolbox::wrap(base(), outcomes(), Vec::new());
        let outcome = tb
            .execute(SUBMIT_RESULT_TOOL, submitted(), "toolu_1")
            .await
            .unwrap();
        assert_eq!(outcome, ToolOutcome::StopRun);
    }

    /// An undeclared outcome would otherwise reach the driver, match no
    /// transition, and end the run as though the step had finished the graph.
    /// Rejecting it here makes it an ordinary tool error the model can fix.
    #[tokio::test]
    async fn an_undeclared_outcome_is_an_input_error_the_model_can_retry() {
        let tb = StepResultToolbox::wrap(base(), outcomes(), Vec::new());
        let err = tb
            .execute(
                SUBMIT_RESULT_TOOL,
                serde_json::json!({"outcome": "maybe", "description": "did it"}),
                "toolu_1",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolCallError::InvalidInput(reason) if reason.contains("'maybe'")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_required_field_is_an_input_error() {
        let fields = vec![StepField {
            name: "files".into(),
            kind: StepFieldType::StringList,
            description: "what changed".into(),
            required: Some(true),
        }];
        let tb = StepResultToolbox::wrap(base(), outcomes(), fields);
        let err = tb
            .execute(SUBMIT_RESULT_TOOL, submitted(), "toolu_1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)), "{err:?}");
    }

    /// The wrapped toolbox must see the model's own call id. Passing a literal
    /// gave every tool call in every step the same one, and that id is what the
    /// runtime correlates a reply by and what a cancel names — so two concurrent
    /// calls from one step collided.
    #[tokio::test]
    async fn the_wrapped_toolbox_sees_the_real_call_id() {
        use std::sync::Mutex;

        struct Recording(Mutex<Vec<String>>);

        #[async_trait]
        impl Toolbox for Recording {
            fn specs(&self) -> Vec<ToolSpec> {
                vec![ToolSpec {
                    name: "noop".into(),
                    description: String::new(),
                    input_schema: serde_json::json!({"type": "object"}),
                }]
            }

            async fn execute(
                &self,
                _name: &str,
                _input: Value,
                tool_call_id: &str,
            ) -> Result<ToolOutcome, ToolCallError> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(tool_call_id.to_string());
                Ok(ToolOutcome::result(Value::Null))
            }
        }

        let inner = Arc::new(Recording(Mutex::new(Vec::new())));
        let tb = StepResultToolbox::wrap(inner.clone(), outcomes(), Vec::new());
        tb.execute("noop", serde_json::json!({}), "toolu_real")
            .await
            .unwrap();
        assert_eq!(
            inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["toolu_real"]
        );
    }
}
