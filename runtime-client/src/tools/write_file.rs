use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{ToolCall, WriteFileInput};
use serde_json::{Value, json};

pub struct WriteFileTool {
    client: RuntimeClient,
}
impl WriteFileTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_string(),
            description: "Create or overwrite a file with the given content. Parent dirs are created as needed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let content = input["content"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'content'".into()))?
            .to_string();
        self.client
            .invoke(ToolCall::WriteFile(WriteFileInput { path, content }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
