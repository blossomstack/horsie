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
            ordinary reply, just answer normally instead of calling this."
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
                    "description": "Optional suggested answers."
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

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if name == ASK_USER_TOOL {
            return Err(ToolCallError::ExecutionFailed(
                "the ask_user tool is terminal and is not executed".to_string(),
            ));
        }
        self.inner.execute(name, input).await
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

        async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
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
    async fn ask_user_is_not_executable() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let err = tb.execute(ASK_USER_TOOL, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn delegates_other_calls_to_inner() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let err = tb.execute("bash", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }
}
