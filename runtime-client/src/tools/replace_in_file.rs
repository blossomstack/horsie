use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{LinesMode, RegexMode, ReplaceInFileInput, ReplaceMode, ToolCall};
use serde_json::{Value, json};

pub struct ReplaceInFileTool {
    client: RuntimeClient,
}
impl ReplaceInFileTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ReplaceInFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "replace_in_file".to_string(),
            description: "Replace in a file by regex pattern or line range.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "replacement": { "type": "string" },
                    "regex": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["path", "replacement"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let replacement = input["replacement"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'replacement'".into()))?
            .to_string();
        let mode = if let Some(pattern) = input["regex"].as_str() {
            ReplaceMode::Regex(RegexMode {
                pattern: pattern.to_string(),
            })
        } else {
            let start_line = input["start_line"].as_u64().ok_or_else(|| {
                ToolCallError::InvalidInput("provide 'regex' or 'start_line'+'end_line'".into())
            })?;
            let end_line = input["end_line"]
                .as_u64()
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'end_line'".into()))?;
            ReplaceMode::Lines(LinesMode {
                start_line,
                end_line,
            })
        };
        self.client
            .invoke(ToolCall::ReplaceInFile(ReplaceInFileInput {
                path,
                replacement,
                mode,
            }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
