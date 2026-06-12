use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{ListFilesInput, ToolCall};
use serde_json::{Value, json};

pub struct ListFilesTool {
    client: RuntimeClient,
}
impl ListFilesTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ListFilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".to_string(),
            description: "List directory contents.".to_string(),
            input_schema: crate::tools::with_workspace(json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            })),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let workspace = crate::tools::workspace_arg(&input);
        self.client
            .invoke(ToolCall::ListFiles(ListFilesInput { path, workspace }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}
