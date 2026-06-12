use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{ReplaceLinesInput, ToolCall};
use serde_json::{Value, json};

pub struct ReplaceLinesTool {
    client: RuntimeClient,
}
impl ReplaceLinesTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ReplaceLinesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "replace_lines".to_string(),
            description: "Replace a 1-based, inclusive range of lines in a file with new \
                content. Use this for positional edits; use find_and_replace to edit by \
                matching content."
                .to_string(),
            input_schema: crate::tools::with_workspace(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "replacement": { "type": "string" }
                },
                "required": ["path", "start_line", "end_line", "replacement"]
            })),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let start_line = input["start_line"]
            .as_u64()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'start_line'".into()))?;
        let end_line = input["end_line"]
            .as_u64()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'end_line'".into()))?;
        let replacement = input["replacement"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'replacement'".into()))?
            .to_string();
        let workspace = crate::tools::workspace_arg(&input);
        self.client
            .invoke(ToolCall::ReplaceLines(ReplaceLinesInput {
                path,
                start_line,
                end_line,
                replacement,
                workspace,
            }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}
