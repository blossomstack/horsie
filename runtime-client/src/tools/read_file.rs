use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{ReadFileInput, ToolCall};
use serde_json::{Value, json};

pub struct ReadFileTool {
    client: RuntimeClient,
}
impl ReadFileTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read file contents, optionally limited to a 1-based line range."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["path"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let start_line = input["start_line"].as_u64();
        let end_line = input["end_line"].as_u64();
        self.client
            .invoke(ToolCall::ReadFile(ReadFileInput {
                path,
                start_line,
                end_line,
            }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
