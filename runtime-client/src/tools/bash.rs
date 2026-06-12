use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{BashInput, ToolCall};
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
            description: "Execute a bash command in the runtime's working directory. \
                Optionally set 'timeout_secs' to bound how long the command may run."
                .to_string(),
            input_schema: crate::tools::with_workspace(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["command"]
            })),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'command'".into()))?
            .to_string();
        let timeout_secs = input["timeout_secs"].as_u64();
        let workspace = crate::tools::workspace_arg(&input);
        self.client
            .invoke(ToolCall::Bash(BashInput {
                command,
                timeout_secs,
                workspace,
            }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
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
    use crate::transport::MockTransport;
    use horsie_models::runtime::ToolOutput;

    #[tokio::test]
    async fn surfaces_stderr_on_success() {
        let tool = BashTool::new(RuntimeClient::new(MockTransport::output(ToolOutput {
            stdout: "out".into(),
            stderr: "a warning".into(),
            exit_code: 0,
        })));
        let v = tool.execute(json!({"command": "x"})).await.unwrap();
        let text = v.as_str().unwrap();
        assert!(text.contains("out"));
        assert!(text.contains("a warning"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_as_error() {
        let tool = BashTool::new(RuntimeClient::new(MockTransport::output(ToolOutput {
            stdout: String::new(),
            stderr: "boom".into(),
            exit_code: 1,
        })));
        let err = tool.execute(json!({"command": "x"})).await.unwrap_err();
        match err {
            ToolCallError::ExecutionFailed(msg) => {
                assert!(msg.contains("status 1"), "msg: {msg}");
                assert!(msg.contains("boom"), "msg: {msg}");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
