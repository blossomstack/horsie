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
/// advertises the tool. `specs` is every box's specs, first spelling of a name
/// winning.
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
        // Deduplicated by name, keeping the first — which is the box `execute`
        // would route to, so the schema the model is shown is the one that will
        // actually run. Advertising both would also be a provider error: every
        // one of them rejects a tool list with a repeated name.
        let mut seen = std::collections::HashSet::new();
        self.boxes
            .iter()
            .flat_map(|b| b.specs())
            .filter(|s| seen.insert(s.name.clone()))
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        for b in &self.boxes {
            if b.specs().iter().any(|s| s.name == name) {
                return b.execute(name, input, tool_call_id).await;
            }
        }
        Err(ToolCallError::InvalidInput(format!(
            "no tool named '{name}'"
        )))
    }
}

/// Plugin-declared MCP tools, called through the runtime.
///
/// The counterpart to [`McpToolbox`], and the difference is *where the client
/// lives*: an admin-configured server is reached from the server process, while
/// a plugin's is reached from the sandbox — because a plugin's `npx …` server is
/// a process that belongs next to the workspace. The tool names are namespaced
/// identically, so an agent, an `allowed_tools` allowlist and a hook matcher all
/// see one vocabulary whichever path a tool came from.
pub struct PluginMcpToolbox {
    client: horsie_runtime_client::RuntimeClient,
    tools: Vec<horsie_models::runtime::PluginMcpTool>,
    credentials: Arc<dyn PluginCredentials>,
}

/// Resolves the bearer for one plugin-declared server.
///
/// The MCP client lives in the runtime; this is what keeps the *credential* on
/// the server, where the refresh token and the consent flow are. `force` is the
/// transport's post-`401` signal, relayed: it must bypass any cache.
#[async_trait]
pub trait PluginCredentials: Send + Sync {
    async fn resolve(&self, server: &str, force: bool) -> Option<String>;
}

/// No credentials at all — every plugin server authenticates with what its own
/// declaration carries, or not at all.
pub struct NoPluginCredentials;

#[async_trait]
impl PluginCredentials for NoPluginCredentials {
    async fn resolve(&self, _server: &str, _force: bool) -> Option<String> {
        None
    }
}

impl PluginMcpToolbox {
    /// Build from an already-discovered tool list. Discovery happens once per
    /// `provide()`, alongside the workspace scan.
    #[must_use]
    pub fn new(
        client: horsie_runtime_client::RuntimeClient,
        tools: Vec<horsie_models::runtime::PluginMcpTool>,
        credentials: Arc<dyn PluginCredentials>,
    ) -> Self {
        Self {
            client,
            tools,
            credentials,
        }
    }

    /// The credential for this tool's server, as the one-element list the
    /// request carries. Only the server being called is sent: a request has no
    /// business shipping tokens for servers it will not touch.
    async fn credentials_for(
        &self,
        tool: &str,
        force: bool,
    ) -> Vec<horsie_models::runtime::McpCredential> {
        let Some(server) = tool
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map(|(server, _)| server)
        else {
            return Vec::new();
        };
        self.credentials
            .resolve(server, force)
            .await
            .map(|bearer| horsie_models::runtime::McpCredential {
                server: server.to_string(),
                bearer,
            })
            .into_iter()
            .collect()
    }
}

#[async_trait]
impl Toolbox for PluginMcpToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                // The schema arrives as the JSON text the server published. A
                // server that publishes something unparseable gets an empty
                // object rather than sinking the whole tool list.
                input_schema: serde_json::from_str(&t.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
            })
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        if !self.tools.iter().any(|t| t.name == name) {
            return Err(ToolCallError::InvalidInput(format!(
                "no plugin MCP tool named '{name}'"
            )));
        }
        let creds = self.credentials_for(name, false).await;
        let first = self
            .client
            .mcp_invoke(tool_call_id, name, input.to_string(), creds)
            .await;
        // A refused credential costs one retry with a forced-fresh token, and
        // exactly one: a server that refuses a fresh token is refusing the
        // user, not the token.
        let outcome = match first {
            Err(horsie_runtime_client::RuntimeCallError::McpRefused(_)) => {
                let fresh = self.credentials_for(name, true).await;
                self.client
                    .mcp_invoke(tool_call_id, name, input.to_string(), fresh)
                    .await
            }
            other => other,
        };
        outcome
            .map(Value::String)
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
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

    async fn execute(
        &self,
        name: &str,
        input: Value,
        _tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
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

    /// A discovered tool list becomes specs the agent can call, with the
    /// server's own JSON Schema carried through.
    #[test]
    fn plugin_mcp_tools_become_specs() {
        let client = horsie_runtime_client::RuntimeClient::new(
            horsie_runtime_client::testkit::MockTransport::ok(""),
            "agent",
        );
        let tb = PluginMcpToolbox::new(
            client,
            vec![
                horsie_models::runtime::PluginMcpTool {
                    name: "mcp__docs__search".into(),
                    description: Some("finds things".into()),
                    input_schema: r#"{"type":"object","properties":{"q":{"type":"string"}}}"#
                        .into(),
                },
                // A server that publishes an unparseable schema still gets a
                // usable tool rather than sinking the whole list.
                horsie_models::runtime::PluginMcpTool {
                    name: "mcp__docs__broken".into(),
                    description: None,
                    input_schema: "not json".into(),
                },
            ],
            Arc::new(NoPluginCredentials),
        );
        let specs = tb.specs();
        assert_eq!(specs[0].name, "mcp__docs__search");
        assert_eq!(specs[0].description, "finds things");
        assert_eq!(specs[0].input_schema["properties"]["q"]["type"], "string");
        assert_eq!(specs[1].input_schema, json!({"type": "object"}));
    }

    /// A name the discovery never advertised is refused here rather than sent
    /// to a runtime that would have to refuse it anyway.
    #[tokio::test]
    async fn an_unknown_plugin_mcp_tool_is_refused_locally() {
        let client = horsie_runtime_client::RuntimeClient::new(
            horsie_runtime_client::testkit::MockTransport::ok(""),
            "agent",
        );
        let tb = PluginMcpToolbox::new(client, Vec::new(), Arc::new(NoPluginCredentials));
        let err = tb
            .execute("mcp__docs__search", json!({}), "tc1")
            .await
            .expect_err("no such tool");
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

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
        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<Value, ToolCallError> {
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
            tb.execute("beta", json!({}), "tc1").await.unwrap(),
            json!("ran beta")
        );
        assert!(matches!(
            tb.execute("gamma", json!({}), "tc1").await,
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
            .execute(
                "mcp__github__create_pull_request",
                json!({ "title": "x" }),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(out, json!("PR #7 opened"));

        // A name outside this server's namespace is rejected without a call.
        assert!(matches!(
            tb.execute("bash", json!({}), "tc1").await,
            Err(ToolCallError::InvalidInput(_))
        ));
    }

    /// An admin-configured server outranks a plugin that declares its name.
    /// `provide()` composes them in this order for exactly this reason: a
    /// plugin must not be able to capture calls — arguments and all — meant for
    /// a server the user configured.
    #[tokio::test]
    async fn an_admin_server_outranks_a_plugin_of_the_same_name() {
        let plugin: Arc<dyn Toolbox> = Arc::new(PluginMcpToolbox::new(
            horsie_runtime_client::RuntimeClient::new(
                horsie_runtime_client::testkit::MockTransport::ok(""),
                "agent",
            ),
            vec![horsie_models::runtime::PluginMcpTool {
                name: "mcp__github__open_pr".into(),
                description: Some("the plugin's".into()),
                input_schema: r#"{"type":"object"}"#.into(),
            }],
            Arc::new(NoPluginCredentials),
        ));
        let admin: Arc<dyn Toolbox> = Arc::new(
            McpToolbox::connect(
                "github".into(),
                mock_client(vec![
                    ("initialize", json!({})),
                    (
                        "tools/list",
                        json!({ "tools": [ { "name": "open_pr", "description": "the admin's",
                                             "inputSchema": { "type": "object" } } ] }),
                    ),
                    (
                        "tools/call",
                        json!({ "content": [ { "type": "text", "text": "from the admin server" } ],
                                "isError": false }),
                    ),
                ]),
            )
            .await
            .unwrap(),
        );
        // The order `provide()` builds: admin boxes first, plugin box appended.
        let tb = CompositeToolbox::new(vec![admin, plugin]);
        // Advertised once — a repeated tool name is a provider error — and it
        // is the admin server's schema, which is the one that will run.
        let specs: Vec<_> = tb
            .specs()
            .into_iter()
            .filter(|s| s.name == "mcp__github__open_pr")
            .collect();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description, "the admin's");
        assert_eq!(
            tb.execute("mcp__github__open_pr", json!({}), "tc1")
                .await
                .unwrap(),
            json!("from the admin server")
        );
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
        match tb.execute("mcp__srv__boom", json!({}), "tc1").await {
            Err(ToolCallError::ExecutionFailed(msg)) => assert_eq!(msg, "kaboom"),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
