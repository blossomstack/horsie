//! Server-side MCP tools as a [`Toolbox`].
//!
//! [`McpToolbox`] adapts a remote MCP server ([`horsie_support::mcp::McpClient`])
//! to the agent's [`Toolbox`] trait; [`CompositeToolbox`] fans several toolboxes
//! into one. Composed into the agent's toolbox next to the runtime tools, MCP
//! calls execute in the server process and never reach the sandbox.

use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use horsie_support::mcp::{McpClient, McpError, McpToolDef};
use serde_json::Value;
use std::sync::Arc;

/// A configured MCP server whose tools are not in this turn's toolbox, and why.
///
/// Both cases used to reach the agent as `no tool named 'mcp__…'`, so "the
/// server is down" and "somebody deleted it" were the same sentence — and the
/// only thing that knew the difference had already dropped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpUnavailable {
    /// Named by the session, but no such server is configured any more.
    Gone { server: String },
    /// Configured, but this turn could not reach it.
    Unreachable { server: String, reason: String },
    /// Configured and reachable, but it has not been authorised.
    NeedsAuth { server: String },
}

impl McpUnavailable {
    #[must_use]
    pub fn server(&self) -> &str {
        match self {
            Self::Gone { server }
            | Self::Unreachable { server, .. }
            | Self::NeedsAuth { server } => server,
        }
    }

    /// What the agent is told when it calls one of this server's tools.
    fn explain(&self) -> String {
        match self {
            Self::Gone { server } => format!(
                "the MCP server '{server}' is no longer configured, so its tools are gone.                  Do not call them again; say so and carry on without them."
            ),
            Self::Unreachable { server, reason } => format!(
                "the MCP server '{server}' could not be reached this turn ({reason}), so its                  tools are unavailable. It may recover on a later turn."
            ),
            Self::NeedsAuth { server } => format!(
                "the MCP server '{server}' has not been authorised, so its tools are                  unavailable until someone connects it in Settings."
            ),
        }
    }
}

/// The server name in `mcp__<server>__<tool>`, for a name that is one.
fn mcp_server_of(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__")?.split("__").next()
}

/// What a turn got from the MCP servers it asked for: the toolboxes that
/// connected, and the servers that did not, with why.
#[derive(Default)]
pub struct McpToolboxes {
    pub boxes: Vec<Arc<dyn Toolbox>>,
    pub unavailable: Vec<McpUnavailable>,
}

impl McpToolboxes {
    /// Whether this turn has anything to say about MCP at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty() && self.unavailable.is_empty()
    }
}

/// Composes several toolboxes into one, routing `execute` to the first box that
/// advertises the tool. `specs` is every box's specs, first spelling of a name
/// winning.
pub struct CompositeToolbox {
    boxes: Vec<Arc<dyn Toolbox>>,
    /// Servers this turn asked for and did not get. Advertised to nobody — a
    /// tool that is not there cannot be offered — but consulted when a call
    /// arrives for one anyway, which is exactly what happens when a server goes
    /// down mid-conversation and the model calls a tool it saw earlier.
    unavailable: Vec<McpUnavailable>,
}

impl CompositeToolbox {
    pub fn new(boxes: Vec<Arc<dyn Toolbox>>) -> Self {
        Self {
            boxes,
            unavailable: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_unavailable(mut self, unavailable: Vec<McpUnavailable>) -> Self {
        self.unavailable = unavailable;
        self
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
        // A call for a server that is configured-but-absent gets the reason
        // rather than "no tool named …", which said nothing about whether it
        // was worth trying again — or whether the tool had ever existed.
        if let Some(server) = mcp_server_of(name)
            && let Some(missing) = self.unavailable.iter().find(|u| u.server() == server)
        {
            return Err(ToolCallError::InvalidInput(missing.explain()));
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
    client: horsie_runtime_host::RuntimeClient,
    tools: Vec<horsie_models::runtime::PluginMcpTool>,
}

impl PluginMcpToolbox {
    /// Build from an already-discovered tool list. Discovery happens once per
    /// `provide()`, alongside the workspace scan.
    #[must_use]
    pub fn new(
        client: horsie_runtime_host::RuntimeClient,
        tools: Vec<horsie_models::runtime::PluginMcpTool>,
    ) -> Self {
        Self { client, tools }
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
        self.client
            .mcp_invoke(tool_call_id, name, input.to_string())
            .await
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
    use horsie_support::mcp::McpTransport;
    use serde_json::json;
    use std::collections::HashMap;

    /// A discovered tool list becomes specs the agent can call, with the
    /// server's own JSON Schema carried through.
    #[test]
    fn plugin_mcp_tools_become_specs() {
        let client = horsie_runtime_host::RuntimeClient::new(
            horsie_runtime_host::testkit::MockTransport::ok(""),
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
        let client = horsie_runtime_host::RuntimeClient::new(
            horsie_runtime_host::testkit::MockTransport::ok(""),
            "agent",
        );
        let tb = PluginMcpToolbox::new(client, Vec::new());
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

    /// The reported case: a server that answered a moment ago is gone or down,
    /// the model calls a tool it saw earlier, and the answer was
    /// `no tool named 'mcp__…'` — which says nothing about whether it was worth
    /// trying again, or whether the tool had ever existed.
    #[tokio::test]
    async fn a_missing_server_answers_with_the_reason_it_is_missing() {
        let deleted = CompositeToolbox::new(vec![Arc::new(OneTool {
            name: "bash".into(),
        })])
        .with_unavailable(vec![McpUnavailable::Gone {
            server: "acme".into(),
        }]);
        let Err(ToolCallError::InvalidInput(said)) =
            deleted.execute("mcp__acme__search", json!({}), "tc1").await
        else {
            panic!("a call for a deleted server must be refused");
        };
        assert!(said.contains("acme"), "{said}");
        assert!(said.contains("no longer configured"), "{said}");

        let down =
            CompositeToolbox::new(Vec::new()).with_unavailable(vec![McpUnavailable::Unreachable {
                server: "acme".into(),
                reason: "connection refused".into(),
            }]);
        let Err(ToolCallError::InvalidInput(said)) =
            down.execute("mcp__acme__search", json!({}), "tc1").await
        else {
            panic!("a call for an unreachable server must be refused");
        };
        assert!(said.contains("connection refused"), "{said}");
        assert!(
            said.contains("later turn"),
            "down is recoverable and deleted is not: {said}"
        );

        // An unrelated name is still just unknown; the explanation is for the
        // server that was asked for, not for every miss.
        let Err(ToolCallError::InvalidInput(said)) =
            down.execute("mcp__other__thing", json!({}), "tc1").await
        else {
            panic!("unknown tools are still refused");
        };
        assert_eq!(said, "no tool named 'mcp__other__thing'");
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
            horsie_runtime_host::RuntimeClient::new(
                horsie_runtime_host::testkit::MockTransport::ok(""),
                "agent",
            ),
            vec![horsie_models::runtime::PluginMcpTool {
                name: "mcp__github__open_pr".into(),
                description: Some("the plugin's".into()),
                input_schema: r#"{"type":"object"}"#.into(),
            }],
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
