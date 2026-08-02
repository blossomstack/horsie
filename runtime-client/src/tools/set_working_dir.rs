use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{SetWorkingDirInput, ToolCall};
use serde_json::{Value, json};

pub struct SetWorkingDirTool {
    client: RuntimeClient,
}
impl SetWorkingDirTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for SetWorkingDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_working_dir".to_string(),
            description: "Set the working directory for all future tool calls in this \
                session — bash commands and relative paths in the file tools alike. \
                'path' may be absolute or relative to the current working directory. \
                Omit 'path' to reset to the default working directory. Persists until \
                reset; other sessions sharing this runtime are unaffected. Returns the \
                new working directory."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"].as_str().map(str::to_string);
        self.client
            .invoke(ToolCall::SetWorkingDir(SetWorkingDirInput { path }))
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
    use crate::testkit::MockTransport;

    #[tokio::test]
    async fn forwards_path() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetWorkingDirTool::new(RuntimeClient::new(
            MockTransport::ok("/ws/sub").observed_by(&probe),
            "test-agent",
        ));
        let v = tool.execute(json!({"path": "sub"})).await.unwrap();
        assert_eq!(v.as_str().unwrap(), "/ws/sub");
        match &probe.invocations()[0] {
            ToolCall::SetWorkingDir(i) => assert_eq!(i.path.as_deref(), Some("sub")),
            other => panic!("expected SetWorkingDir, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_omitted_path_stays_none_for_reset() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetWorkingDirTool::new(RuntimeClient::new(
            MockTransport::ok("").observed_by(&probe),
            "test-agent",
        ));
        tool.execute(json!({})).await.unwrap();
        match &probe.invocations()[0] {
            ToolCall::SetWorkingDir(i) => assert!(i.path.is_none()),
            other => panic!("expected SetWorkingDir, got {other:?}"),
        }
    }

    #[test]
    fn spec_shape() {
        let tool = SetWorkingDirTool::new(RuntimeClient::new(MockTransport::ok(""), "test-agent"));
        let spec = tool.spec();
        assert_eq!(spec.name, "set_working_dir");
        assert!(spec.input_schema["required"].is_null());
    }
}
