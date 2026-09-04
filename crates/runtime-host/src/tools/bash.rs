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
            description: "Execute a bash command in the runtime's persistent working \
                directory (change it with set_working_dir; do not prefix the command \
                with `cd`). \
                Optionally set 'timeout_secs' to bound how long the command may run. \
                Pipelines run with pipefail: the command fails if any stage fails, \
                except when a consumer closes early (`| head`), which is not a failure. \
                Oversized output is head/tail truncated and saved to a temporary file for follow-up reads. On timeout, output \
                captured so far is returned with the error."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<Value, ToolCallError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'command'".into()))?
            .to_string();
        let timeout_secs = input["timeout_secs"].as_u64();
        self.client
            .invoke(
                tool_call_id,
                ToolCall::Bash(BashInput {
                    command,
                    timeout_secs,
                }),
            )
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_command_output)
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
    use crate::testkit::MockTransport;
    use horsie_models::runtime::ToolOutput;

    #[test]
    fn the_description_names_the_cwd_tool_and_forbids_cd() {
        let tool = BashTool::new(RuntimeClient::detached(MockTransport::ok(""), "test-agent"));
        let d = tool.spec().description;
        assert!(d.contains("set_working_dir"), "{d}");
        assert!(d.contains("do not prefix the command with `cd`"), "{d}");
    }

    #[tokio::test]
    async fn surfaces_stderr_on_success() {
        let tool = BashTool::new(RuntimeClient::detached(
            MockTransport::output(ToolOutput {
                stdout: "out".into(),
                stderr: "a warning".into(),
                exit_code: 0,
                artifacts: Vec::new(),
            }),
            "test-agent",
        ));
        let v = tool.execute(json!({"command": "x"}), "tc1").await.unwrap();
        let text = v.as_str().unwrap();
        assert!(text.contains("out"));
        assert!(text.contains("a warning"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_as_error() {
        let tool = BashTool::new(RuntimeClient::detached(
            MockTransport::output(ToolOutput {
                stdout: String::new(),
                stderr: "boom".into(),
                exit_code: 1,
                artifacts: Vec::new(),
            }),
            "test-agent",
        ));
        let err = tool
            .execute(json!({"command": "x"}), "tc1")
            .await
            .unwrap_err();
        match err {
            ToolCallError::CommandFailed(failure) => {
                assert_eq!(failure.exit_code, 1);
                assert!(failure.to_string().contains("boom"));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extracts_rust_diagnostics_from_cargo_output() {
        let tool = BashTool::new(RuntimeClient::detached(
            MockTransport::output(ToolOutput {
                stdout: String::new(),
                stderr: "error[E0425]: cannot find value `x` in this scope\n  --> src/lib.rs:12:5\n   |\n12 | x;\n   | ^ not found\nerror: could not compile `demo`"
                    .into(),
                exit_code: 101,
                artifacts: Vec::new(),
            }),
            "test-agent",
        ));
        let err = tool
            .execute(json!({"command": "cargo check"}), "tc1")
            .await
            .unwrap_err();
        let ToolCallError::CommandFailed(failure) = err else {
            panic!("expected command failure");
        };
        assert_eq!(failure.diagnostics.len(), 1);
        assert_eq!(failure.diagnostics[0].code.as_deref(), Some("E0425"));
        assert_eq!(
            failure.diagnostics[0].location.as_deref(),
            Some("src/lib.rs:12:5")
        );
    }
}
