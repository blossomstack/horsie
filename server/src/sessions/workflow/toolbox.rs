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

use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use horsie_workflow::{CONCLUDE_TOOL, conclude_tool_spec};
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

    async fn execute(
        &self,
        name: &str,
        input: Value,
        _tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        if name == CONCLUDE_TOOL && self.conclude.is_some() {
            return Err(ToolCallError::ExecutionFailed(
                "the conclude tool is terminal and is not executed".to_string(),
            ));
        }
        self.inner.execute(name, input, "tc1").await
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

    #[tokio::test]
    async fn conclude_is_terminal_and_never_executes() {
        let schema = serde_json::json!({"type": "object"});
        let tb = StepConcludeToolbox::wrap(base(), Some(&schema), false);
        let err = tb
            .execute(CONCLUDE_TOOL, serde_json::json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }
}
