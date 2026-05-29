use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{GlobInput, ToolCall};
use serde_json::{Value, json};

pub struct GlobTool {
    client: RuntimeClient,
}
impl GlobTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_string(),
            description: "Find files by glob pattern.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "max_results": { "type": "integer" }
                },
                "required": ["pattern"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'pattern'".into()))?
            .to_string();
        let path = input["path"].as_str().map(|s| s.to_string());
        let max_results = input["max_results"].as_u64();
        self.client
            .invoke(ToolCall::Glob(GlobInput {
                pattern,
                path,
                max_results,
            }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
