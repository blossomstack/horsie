//! A dedicated "ask the user" tool for interactive horsie sessions.
//!
//! Kept entirely separate from the workflow crate's `conclude` tool, which
//! serves a different purpose (a workflow sub-agent's *forced* way to signal
//! it's done, optionally carrying structured output). Horsie sessions always
//! offer this tool, but never force it: the model may call it to pause for a
//! clarifying question, or just answer normally, freely either way — see
//! `AgentParams::optional_handoff_tool` in the workflow crate, which recognizes
//! a call to it as a handoff without ever forcing `tool_choice`.

use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;

/// Name of the always-available "ask the user" tool for interactive sessions.
pub const ASK_USER_TOOL: &str = "ask_user";

fn ask_user_spec() -> ToolSpec {
    ToolSpec {
        name: ASK_USER_TOOL.to_string(),
        description: "Pause and ask the user a clarifying question before continuing, when \
            their intent is ambiguous or a decision needs their input. Optional -- for an \
            ordinary reply, just answer normally instead of calling this. Omit `choices` for \
            an open question; supply `choices` to suggest answers, and set `multiple` when \
            several may be picked at once."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["question"],
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the user."
                },
                "choices": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional suggested answers. The user can always reply in \
                        their own words instead, so treat these as suggestions and expect an \
                        answer that is not in the list."
                },
                "multiple": {
                    "type": "boolean",
                    "description": "Set true when the user may pick any number of the \
                        choices; omit or set false when exactly one applies. Has no effect \
                        without `choices`."
                }
            }
        }),
    }
}

/// Wraps an inner toolbox, adding the always-present `ask_user` tool. Like the
/// workflow crate's `conclude` tool, a call to it is terminal — the agent loop
/// recognizes it as a handoff (via `with_handoff_tool_optional`) and it is never
/// actually executed here.
pub struct AskUserToolbox {
    inner: Arc<dyn Toolbox>,
}

impl AskUserToolbox {
    pub fn new(inner: Arc<dyn Toolbox>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Toolbox for AskUserToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(ask_user_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        if name == ASK_USER_TOOL {
            return Err(ToolCallError::ExecutionFailed(
                "the ask_user tool is terminal and is not executed".to_string(),
            ));
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    struct EmptyToolbox;

    #[async_trait]
    impl Toolbox for EmptyToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![]
        }

        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<Value, ToolCallError> {
            Err(ToolCallError::InvalidInput(format!(
                "no tool named '{name}'"
            )))
        }
    }

    #[tokio::test]
    async fn adds_ask_user_alongside_inner_specs() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec![ASK_USER_TOOL.to_string()]);
    }

    #[tokio::test]
    async fn spec_offers_multi_select_and_advertises_the_free_text_fallback() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let spec = tb
            .specs()
            .into_iter()
            .find(|s| s.name == ASK_USER_TOOL)
            .expect("ask_user is offered");
        let props = spec
            .input_schema
            .get("properties")
            .expect("schema has properties");

        assert_eq!(
            props.get("multiple").and_then(|m| m.get("type")),
            Some(&json!("boolean")),
            "multi-select must be expressible"
        );
        // `question` stays the only required field: choices and multiple are both
        // optional, so a plain free-text question is still one field.
        assert_eq!(
            spec.input_schema.get("required"),
            Some(&json!(["question"]))
        );

        let choices_doc = props
            .get("choices")
            .and_then(|c| c.get("description"))
            .and_then(Value::as_str)
            .expect("choices is documented");
        assert!(
            choices_doc.contains("own words"),
            "the model must be told choices are suggestions, not a constraint: {choices_doc}"
        );
    }

    #[tokio::test]
    async fn ask_user_is_not_executable() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let err = tb
            .execute(ASK_USER_TOOL, json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn delegates_other_calls_to_inner() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let err = tb.execute("bash", json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }
}
