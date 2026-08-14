use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use horsie_models::runtime::{SetEnvInput, ToolCall};
use serde_json::{Value, json};

pub struct SetEnvTool {
    client: RuntimeClient,
}
impl SetEnvTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for SetEnvTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_env".to_string(),
            description: "Set or unset an environment variable for this session's future \
                bash commands. Omit 'value' to unset — the variable is removed even if \
                the runtime process defines it. Persists until changed again; file tools \
                are unaffected, and so are other sessions sharing this runtime."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "value": { "type": "string" }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<Value, ToolCallError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'name'".into()))?
            .to_string();
        let value = input["value"].as_str().map(str::to_string);
        self.client
            .invoke(tool_call_id, ToolCall::SetEnv(SetEnvInput { name, value }))
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
    async fn forwards_name_and_value() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetEnvTool::new(RuntimeClient::detached(
            MockTransport::ok("set FOO").observed_by(&probe),
            "test-agent",
        ));
        let v = tool
            .execute(json!({"name": "FOO", "value": "1"}), "tc1")
            .await
            .unwrap();
        assert_eq!(v.as_str().unwrap(), "set FOO");
        match &probe.invocations()[0] {
            ToolCall::SetEnv(i) => {
                assert_eq!(i.name, "FOO");
                assert_eq!(i.value.as_deref(), Some("1"));
            }
            other => panic!("expected SetEnv, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn omitted_value_means_unset() {
        let probe = crate::testkit::TransportProbe::new();
        let tool = SetEnvTool::new(RuntimeClient::detached(
            MockTransport::ok("").observed_by(&probe),
            "test-agent",
        ));
        tool.execute(json!({"name": "FOO"}), "tc1").await.unwrap();
        match &probe.invocations()[0] {
            ToolCall::SetEnv(i) => assert!(i.value.is_none()),
            other => panic!("expected SetEnv, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_name_is_an_input_error() {
        let tool = SetEnvTool::new(RuntimeClient::detached(MockTransport::ok(""), "test-agent"));
        let err = tool.execute(json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn spec_shape() {
        let tool = SetEnvTool::new(RuntimeClient::detached(MockTransport::ok(""), "test-agent"));
        let spec = tool.spec();
        assert_eq!(spec.name, "set_env");
        assert_eq!(spec.input_schema["required"], json!(["name"]));
        assert!(spec.input_schema["properties"].get("workspace").is_none());
    }
}
