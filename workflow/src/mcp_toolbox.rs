//! Server-side MCP tools as a [`Toolbox`].
//!
//! [`McpToolbox`] adapts a remote MCP server ([`horsie_mcp_client::McpClient`])
//! to the agent's [`Toolbox`] trait; [`CompositeToolbox`] fans several toolboxes
//! into one. Composed into the agent's toolbox next to the runtime tools, MCP
//! calls execute in the server process and never reach the sandbox.

use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use horsie_mcp_client::{McpClient, McpError, McpToolDef};
use serde_json::Value;
use std::sync::Arc;

/// Composes several toolboxes into one, routing `execute` to the first box that
/// advertises the tool. `specs` is the concatenation of all boxes' specs.
pub struct CompositeToolbox {
    boxes: Vec<Arc<dyn Toolbox>>,
}

impl CompositeToolbox {
    pub fn new(boxes: Vec<Arc<dyn Toolbox>>) -> Self {
        Self { boxes }
    }
}

#[async_trait]
impl Toolbox for CompositeToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.boxes.iter().flat_map(|b| b.specs()).collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        for b in &self.boxes {
            if b.specs().iter().any(|s| s.name == name) {
                return b.execute(name, input).await;
            }
        }
        Err(ToolCallError::InvalidInput(format!(
            "no tool named '{name}'"
        )))
    }
}

/// A toolbox backed by a remote MCP server. Tools are namespaced
/// `mcp__<server>__<tool>` so they never collide with runtime tools and can be
/// selected through the agent's `allowed_tools` allowlist.
pub struct McpToolbox {
    server: String,
    client: Arc<McpClient>,
    tools: Vec<McpToolDef>,
}

impl McpToolbox {
    /// Build from an already-fetched tool list (see [`McpToolbox::connect`]).
    pub fn new(server: String, client: Arc<McpClient>, tools: Vec<McpToolDef>) -> Self {
        Self {
            server,
            client,
            tools,
        }
    }

    /// Connect: `initialize` + `tools/list`, capturing the advertised tools.
    pub async fn connect(server: String, client: Arc<McpClient>) -> Result<Self, McpError> {
        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok(Self::new(server, client, tools))
    }

    fn prefix(&self) -> String {
        format!("mcp__{}__", self.server)
    }
}

#[async_trait]
impl Toolbox for McpToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let prefix = self.prefix();
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: format!("{prefix}{}", t.name),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        let prefix = self.prefix();
        let tool = name.strip_prefix(&prefix).ok_or_else(|| {
            ToolCallError::InvalidInput(format!(
                "'{name}' is not a tool of MCP server '{}'",
                self.server
            ))
        })?;
        match self.client.call_tool(tool, input).await {
            Ok(outcome) if outcome.is_error => Err(ToolCallError::ExecutionFailed(outcome.text)),
            Ok(outcome) => Ok(Value::String(outcome.text)),
            Err(e) => Err(ToolCallError::ExecutionFailed(e.to_string())),
        }
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
    use horsie_mcp_client::McpTransport;
    use serde_json::json;
    use std::collections::HashMap;

    /// A one-tool toolbox for exercising `CompositeToolbox` routing.
    struct OneTool {
        name: String,
    }

    #[async_trait]
    impl Toolbox for OneTool {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: self.name.clone(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }]
        }
        async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
            Ok(Value::String(format!("ran {name}")))
        }
    }

    /// A transport that answers each method from a canned `result` map.
    struct MockTransport {
        results: HashMap<String, Value>,
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            self.results
                .get(method)
                .cloned()
                .ok_or_else(|| McpError::Protocol(format!("no mock for {method}")))
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
    }

    fn mock_client(results: Vec<(&str, Value)>) -> Arc<McpClient> {
        let map = results
            .into_iter()
            .map(|(m, v)| (m.to_string(), v))
            .collect();
        Arc::new(McpClient::new(Arc::new(MockTransport { results: map })))
    }

    #[tokio::test]
    async fn composite_unions_specs_and_routes_by_name() {
        let tb = CompositeToolbox::new(vec![
            Arc::new(OneTool {
                name: "alpha".into(),
            }),
            Arc::new(OneTool {
                name: "beta".into(),
            }),
        ]);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(
            tb.execute("beta", json!({})).await.unwrap(),
            json!("ran beta")
        );
        assert!(matches!(
            tb.execute("gamma", json!({})).await,
            Err(ToolCallError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn mcp_toolbox_namespaces_specs_and_executes() {
        let client = mock_client(vec![
            ("initialize", json!({})),
            (
                "tools/list",
                json!({ "tools": [ { "name": "create_pull_request", "description": "open a PR", "inputSchema": { "type": "object" } } ] }),
            ),
            (
                "tools/call",
                json!({ "content": [ { "type": "text", "text": "PR #7 opened" } ], "isError": false }),
            ),
        ]);
        let tb = McpToolbox::connect("github".into(), client).await.unwrap();

        let specs = tb.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "mcp__github__create_pull_request");
        assert_eq!(specs[0].description, "open a PR");

        let out = tb
            .execute("mcp__github__create_pull_request", json!({ "title": "x" }))
            .await
            .unwrap();
        assert_eq!(out, json!("PR #7 opened"));

        // A name outside this server's namespace is rejected without a call.
        assert!(matches!(
            tb.execute("bash", json!({})).await,
            Err(ToolCallError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn mcp_toolbox_maps_is_error_to_execution_failed() {
        let client = mock_client(vec![
            ("initialize", json!({})),
            (
                "tools/list",
                json!({ "tools": [ { "name": "boom", "inputSchema": { "type": "object" } } ] }),
            ),
            (
                "tools/call",
                json!({ "content": [ { "type": "text", "text": "kaboom" } ], "isError": true }),
            ),
        ]);
        let tb = McpToolbox::connect("srv".into(), client).await.unwrap();
        match tb.execute("mcp__srv__boom", json!({})).await {
            Err(ToolCallError::ExecutionFailed(msg)) => assert_eq!(msg, "kaboom"),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
