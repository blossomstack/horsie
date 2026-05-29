use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{EditFileInput, ToolCall};
use serde_json::{Value, json};

pub struct EditFileTool {
    client: RuntimeClient,
}
impl EditFileTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".to_string(),
            description: "Replace the first occurrence of old_text with new_text in a file."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let old_text = input["old_text"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'old_text'".into()))?
            .to_string();
        let new_text = input["new_text"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'new_text'".into()))?
            .to_string();
        self.client
            .invoke(ToolCall::EditFile(EditFileInput {
                path,
                old_text,
                new_text,
            }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
