use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{BashInput, ToolCall};
use serde_json::{Value, json};

pub struct BashTool {
    client: RuntimeClient,
}

impl BashTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description: "Execute a bash command in the runtime's working directory.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'command'".into()))?
            .to_string();
        self.client
            .invoke(ToolCall::Bash(BashInput { command }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
