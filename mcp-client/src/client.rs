use crate::error::McpError;
use crate::transport::McpTransport;
use crate::types::{McpCallOutcome, McpToolDef};
use serde_json::{Value, json};
use std::sync::Arc;

/// The MCP protocol version this client advertises on `initialize`.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// A connection to one remote MCP server: `initialize`, `tools/list`,
/// `tools/call`, over a pluggable [`McpTransport`].
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
}

impl McpClient {
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self { transport }
    }

    /// Perform the MCP handshake: `initialize`, then the
    /// `notifications/initialized` notification.
    pub async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "horsie", "version": env!("CARGO_PKG_VERSION") },
        });
        self.transport.request("initialize", params).await?;
        self.transport
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(())
    }

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.transport.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol("tools/list result missing 'tools'".to_string()))?;
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| McpError::Protocol("tool missing 'name'".to_string()))?
                .to_string();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input_schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            out.push(McpToolDef {
                name,
                description,
                input_schema,
            });
        }
        Ok(out)
    }

    /// Call a tool by its MCP name, returning the joined text content and the
    /// `isError` flag.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpCallOutcome, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.transport.request("tools/call", params).await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(McpCallOutcome {
            is_error,
            text: extract_text(&result),
        })
    }
}

/// Join the `text` fields of a `tools/call` result's `content[]` blocks.
fn extract_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = block.get("text").and_then(Value::as_str)
        {
            parts.push(t.to_string());
        }
    }
    parts.join("\n")
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
    use crate::transport::McpTransport;
    use async_trait::async_trait;
    use std::collections::HashMap;

    /// A transport that answers each method from a canned `result` map.
    struct MockTransport {
        results: HashMap<String, Value>,
    }

    impl MockTransport {
        fn new(results: Vec<(&str, Value)>) -> Self {
            Self {
                results: results
                    .into_iter()
                    .map(|(m, v)| (m.to_string(), v))
                    .collect(),
            }
        }
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

    fn client(results: Vec<(&str, Value)>) -> McpClient {
        McpClient::new(Arc::new(MockTransport::new(results)))
    }

    #[tokio::test]
    async fn initialize_sends_handshake() {
        let c = client(vec![(
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION }),
        )]);
        c.initialize().await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_parses_definitions() {
        let c = client(vec![(
            "tools/list",
            json!({ "tools": [
                { "name": "create_pull_request", "description": "open a PR", "inputSchema": { "type": "object", "properties": { "title": { "type": "string" } } } },
                { "name": "bare" }
            ] }),
        )]);
        let tools = c.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "create_pull_request");
        assert_eq!(tools[0].description, "open a PR");
        assert_eq!(
            tools[0].input_schema["properties"]["title"]["type"],
            "string"
        );
        // Missing description/schema get sane defaults.
        assert_eq!(tools[1].description, "");
        assert_eq!(tools[1].input_schema, json!({ "type": "object" }));
    }

    #[tokio::test]
    async fn call_tool_joins_text_and_reads_is_error() {
        let c = client(vec![(
            "tools/call",
            json!({ "content": [ { "type": "text", "text": "line 1" }, { "type": "text", "text": "line 2" } ], "isError": false }),
        )]);
        let out = c.call_tool("t", json!({})).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, "line 1\nline 2");
    }

    #[tokio::test]
    async fn call_tool_surfaces_is_error() {
        let c = client(vec![(
            "tools/call",
            json!({ "content": [ { "type": "text", "text": "boom" } ], "isError": true }),
        )]);
        let out = c.call_tool("t", json!({})).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }

    #[tokio::test]
    async fn list_tools_without_tools_array_errors() {
        let c = client(vec![("tools/list", json!({}))]);
        assert!(matches!(c.list_tools().await, Err(McpError::Protocol(_))));
    }
}
