//! The terminal tool a workflow step finishes with.
//!
//! An interactive agent ends its turn with plain text and asks through
//! `ask_user`. A step does neither: it finishes by calling `conclude`, whose
//! input schema *is* the step's declared output schema — which is what makes a
//! transition condition able to read `output.severity` at all.
//!
//! The tool is never executed. Naming it as the agent loop's handoff tool is
//! what makes the call terminal, so reaching `execute` means something upstream
//! stopped treating it as one.

use crate::agent_loop::{CONCLUDE_TOOL, conclude_tool_spec};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// Wraps a step's toolbox, adding `conclude`.
pub struct StepConcludeToolbox {
    inner: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
}

impl StepConcludeToolbox {
    /// `output_schema` is the step's; `allow_ask` widens `conclude` into a
    /// kind-tagged union so the step can pause for a question instead of
    /// submitting. Returns the inner toolbox unchanged when the step declares
    /// neither — such a step ends its turn with plain text, and that text is
    /// its output.
    pub fn wrap(
        inner: Arc<dyn Toolbox>,
        output_schema: Option<&Value>,
        allow_ask: bool,
    ) -> Arc<dyn Toolbox> {
        match conclude_tool_spec(output_schema, allow_ask, false) {
            None => inner,
            Some(conclude) => Arc::new(Self {
                inner,
                conclude: Some(conclude),
            }),
        }
    }
}

#[async_trait]
impl Toolbox for StepConcludeToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        if let Some(c) = &self.conclude {
            specs.push(c.clone());
        }
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
        if name == CONCLUDE_TOOL && self.conclude.is_some() {
            return Ok(ToolOutcome::StopRun);
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_agentcore::ToolboxImpl;

    fn base() -> Arc<dyn Toolbox> {
        Arc::new(ToolboxImpl::new())
    }

    /// The tool the loop is told to watch for has to be in the toolbox, or the
    /// run fails with "handoff tool 'conclude' is not present".
    #[test]
    fn a_step_with_an_output_schema_advertises_conclude() {
        let schema = serde_json::json!({"type": "object"});
        let tb = StepConcludeToolbox::wrap(base(), Some(&schema), false);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&CONCLUDE_TOOL.to_string()));
    }

    /// With an output schema alone the payload *is* the output; adding the
    /// ability to ask makes it a kind-tagged union, and the output nests.
    #[test]
    fn allowing_an_ask_makes_the_payload_kind_tagged() {
        let schema = serde_json::json!({"type": "object"});
        let tb = StepConcludeToolbox::wrap(base(), Some(&schema), true);
        let spec = tb
            .specs()
            .into_iter()
            .find(|s| s.name == CONCLUDE_TOOL)
            .unwrap();
        assert_eq!(spec.input_schema["properties"]["kind"]["enum"][0], "submit");
        assert!(spec.input_schema["properties"]["output"].is_object());
    }

    #[test]
    fn a_step_with_neither_gets_no_conclude_and_ends_with_text() {
        let tb = StepConcludeToolbox::wrap(base(), None, false);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(!names.contains(&CONCLUDE_TOOL.to_string()));
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
                Ok(ToolOutcome::Result(Value::Null))
            }
        }

        let inner = Arc::new(Recording(Mutex::new(Vec::new())));
        let schema = serde_json::json!({"type": "object"});
        let tb = StepConcludeToolbox::wrap(inner.clone(), Some(&schema), false);
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

    #[tokio::test]
    async fn conclude_stops_the_run() {
        let schema = serde_json::json!({"type": "object"});
        let tb = StepConcludeToolbox::wrap(base(), Some(&schema), false);
        let outcome = tb
            .execute(CONCLUDE_TOOL, serde_json::json!({}), "tc1")
            .await
            .unwrap();
        assert_eq!(outcome, ToolOutcome::StopRun);
    }
}
