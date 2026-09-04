//! Server-side MCP tools as a [`Toolbox`].
//!
//! [`McpToolbox`] adapts a remote MCP server
//! ([`horsie_support::mcp::McpClient`]) to the agent's [`Toolbox`] trait;
//! [`CompositeToolbox`] fans several toolboxes into one. Composed into the
//! agent's toolbox next to the runtime tools, MCP calls execute in the server
//! process and never reach the sandbox.

use crate::artifacts::ArtifactService;
use crate::projects::ProjectId;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, ToolValue, Toolbox};
use horsie_models::agent::ArtifactRef;
use horsie_support::mcp::{McpClient, McpError, McpImage, McpServerInfo, McpToolDef};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Where a toolbox puts the bytes a tool produced: one project's artifact
/// store.
///
/// A pair rather than two parameters, because a service without the project it
/// is storing into cannot be used and the two always travel together. It is
/// also the only thing a toolbox is given — it can store bytes and read nothing
/// back, which is the whole of what a tool result needs.
#[derive(Clone)]
pub struct ArtifactSink {
    service: Arc<ArtifactService>,
    project: ProjectId,
}

impl ArtifactSink {
    #[must_use]
    pub fn new(service: Arc<ArtifactService>, project: ProjectId) -> Self {
        Self { service, project }
    }

    /// Store one named blob, returning the message-safe reference.
    pub async fn store_one(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
    ) -> Result<ArtifactRef, String> {
        self.service
            .put(&self.project, bytes, filename)
            .await
            .map_err(|error| error.to_string())
    }

    /// Store every blob, keeping the ones that made it.
    ///
    /// A blob that will not store is logged and skipped rather than failing the
    /// call: losing a screenshot is much better than losing the whole result,
    /// and the text beside it is usually the part the model was going to act
    /// on. The service refuses anything that is not an image or a PDF, so a
    /// tool answering with some other binary loses only that block.
    async fn store(&self, blobs: impl IntoIterator<Item = Vec<u8>>) -> Vec<ArtifactRef> {
        let mut refs = Vec::new();
        for bytes in blobs {
            match self.service.put(&self.project, bytes, None).await {
                Ok(r) => refs.push(r),
                Err(e) => tracing::warn!(
                    error = %e,
                    "an MCP tool result's artifact could not be stored; keeping the text"
                ),
            }
        }
        refs
    }
}

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
    /// down mid-session and the model calls a tool it saw earlier.
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
    ) -> Result<ToolOutcome, ToolCallError> {
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
/// lives*: an admin-configured server is reached from the server process,
/// while a plugin's is reached from the sandbox — because a plugin's `npx …`
/// server is a process that belongs next to the workspace. The tool names are
/// namespaced identically, so an agent, an `allowed_tools` allowlist and a
/// hook matcher all see one vocabulary whichever path a tool came from.
pub struct PluginMcpToolbox {
    client: horsie_runtime_host::RuntimeClient,
    tools: Vec<horsie_models::runtime::PluginMcpTool>,
    /// Where the runtime's bytes are stored. The runtime has no database, so it
    /// ships raw bytes and this is what turns them into references.
    ///
    /// `None` only where there is no project to store into — a harness that
    /// wired an agent without services. Such a toolbox keeps the text and drops
    /// the bytes, which is what it did before any of this existed.
    artifacts: Option<ArtifactSink>,
}

impl PluginMcpToolbox {
    /// Build from an already-discovered tool list. Discovery happens once per
    /// `provide()`, alongside the workspace scan.
    #[must_use]
    pub fn new(
        client: horsie_runtime_host::RuntimeClient,
        tools: Vec<horsie_models::runtime::PluginMcpTool>,
        artifacts: Option<ArtifactSink>,
    ) -> Self {
        Self {
            client,
            tools,
            artifacts,
        }
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
    ) -> Result<ToolOutcome, ToolCallError> {
        if !self.tools.iter().any(|t| t.name == name) {
            return Err(ToolCallError::InvalidInput(format!(
                "no plugin MCP tool named '{name}'"
            )));
        }
        let output = self
            .client
            .mcp_invoke(tool_call_id, name, input.to_string())
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
        let refs = match (&self.artifacts, output.artifacts.is_empty()) {
            (Some(sink), false) => {
                sink.store(output.artifacts.into_iter().map(Vec::from))
                    .await
            }
            _ => Vec::new(),
        };
        Ok(ToolOutcome::Result(ToolValue::with_artifacts(
            Value::String(output.stdout),
            refs,
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
    /// What the server said about itself when this toolbox connected.
    info: McpServerInfo,
    /// The tools of this server that are in scope, by their unprefixed names.
    ///
    /// `None` is "all of them", which is not `Some(every name)`: a server that
    /// gains a tool must reach a session that asked for the whole server, and
    /// a frozen list of names would quietly stop that happening.
    ///
    /// This is the only layer that can do the filtering. The session-wide
    /// `FilteredToolbox` deliberately passes names it does not recognise —
    /// which is every MCP name there is — and it is a flat set with no idea
    /// which server a name came from.
    allowed: Option<BTreeSet<String>>,
    /// Where an image block's bytes go. No runtime hop here: the client is in
    /// this process, so the bytes are already in hand when the call returns.
    artifacts: ArtifactSink,
}

impl McpToolbox {
    /// Build from an already-fetched tool list (see [`McpToolbox::connect`]).
    pub fn new(
        server: String,
        client: Arc<McpClient>,
        info: McpServerInfo,
        tools: Vec<McpToolDef>,
        artifacts: ArtifactSink,
    ) -> Self {
        Self {
            server,
            client,
            tools,
            info,
            allowed: None,
            artifacts,
        }
    }

    /// Narrow this toolbox to some of the server's tools. `None` leaves it
    /// whole.
    #[must_use]
    pub fn with_allowed(mut self, allowed: Option<&[String]>) -> Self {
        self.allowed = allowed.map(|names| names.iter().cloned().collect());
        self
    }

    /// Connect: `initialize` + `tools/list`, capturing what the server says it
    /// is along with the tools it advertises.
    pub async fn connect(
        server: String,
        client: Arc<McpClient>,
        artifacts: ArtifactSink,
    ) -> Result<Self, McpError> {
        let info = client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok(Self::new(server, client, info, tools, artifacts))
    }

    /// Whether a tool of this server, named as the server names it, is in
    /// scope for this session.
    fn is_allowed(&self, tool: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(tool))
    }

    /// What the server said about itself on this connection.
    #[must_use]
    pub fn info(&self) -> &McpServerInfo {
        &self.info
    }

    /// Every tool the server advertised, in its own spelling (unprefixed) —
    /// deliberately unfiltered. This is what gets remembered as the server's
    /// catalogue, and the catalogue is a fact about the server, not about
    /// whichever session happened to connect this turn.
    #[must_use]
    pub fn tool_defs(&self) -> &[McpToolDef] {
        &self.tools
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
            .filter(|t| self.is_allowed(&t.name))
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
    ) -> Result<ToolOutcome, ToolCallError> {
        let prefix = self.prefix();
        let tool = name.strip_prefix(&prefix).ok_or_else(|| {
            ToolCallError::InvalidInput(format!(
                "'{name}' is not a tool of MCP server '{}'",
                self.server
            ))
        })?;
        // A narrowed server refuses in its own words. "Not selected" and "no
        // such tool" are different situations, and collapsing them leaves a
        // model retrying a name that exists and will never be allowed —
        // exactly the confusion `McpUnavailable` exists to prevent one layer
        // up.
        if !self.is_allowed(tool) {
            return Err(ToolCallError::InvalidInput(format!(
                "'{tool}' is not among the tools selected from MCP server '{}' for this session, \
                 so it cannot be called. Do not try it again.",
                self.server
            )));
        }
        match self.client.call_tool(tool, input).await {
            Ok(outcome) if outcome.is_error => Err(ToolCallError::ExecutionFailed(outcome.text)),
            Ok(outcome) => {
                let refs = self
                    .artifacts
                    .store(outcome.images.into_iter().map(|McpImage(b)| b))
                    .await;
                Ok(ToolOutcome::Result(ToolValue::with_artifacts(
                    Value::String(outcome.text),
                    refs,
                )))
            }
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

    /// A sink over a real database. The `Db` is returned so the caller holds it
    /// open for the length of the test.
    async fn sink() -> (ArtifactSink, crate::db::Db) {
        let db = crate::db::testing::db().await;
        let service = Arc::new(crate::artifacts::ArtifactService::in_database(db.clone()));
        (ArtifactSink::new(service, ProjectId::new("p1")), db)
    }

    /// A real 1x1 PNG — the artifact service decides what bytes are from the
    /// bytes, so a test that wants a stored image has to hand it a real one.
    fn png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00,
        ]
    }

    /// `data` for an MCP image block.
    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A discovered tool list becomes specs the agent can call, with the
    /// server's own JSON Schema carried through.
    #[test]
    fn plugin_mcp_tools_become_specs() {
        let client = horsie_runtime_host::RuntimeClient::detached(
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
            None,
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
        let client = horsie_runtime_host::RuntimeClient::detached(
            horsie_runtime_host::testkit::MockTransport::ok(""),
            "agent",
        );
        let tb = PluginMcpToolbox::new(client, Vec::new(), None);
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
        ) -> Result<ToolOutcome, ToolCallError> {
            Ok(ToolOutcome::result(Value::String(format!("ran {name}"))))
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

    /// A three-tool server, for the narrowing tests.
    async fn linear() -> (McpToolbox, crate::db::Db) {
        let (sink, db) = sink().await;
        let client = mock_client(vec![
            ("initialize", json!({})),
            (
                "tools/list",
                json!({ "tools": [
                    { "name": "search_issues", "description": "find" },
                    { "name": "create_issue", "description": "open" },
                    { "name": "delete_issue", "description": "shred" }
                ] }),
            ),
            (
                "tools/call",
                json!({ "content": [{ "type": "text", "text": "ok" }] }),
            ),
        ]);
        let tb = McpToolbox::connect("linear".into(), client, sink)
            .await
            .unwrap();
        (tb, db)
    }

    fn spec_names(tb: &McpToolbox) -> Vec<String> {
        tb.specs().into_iter().map(|s| s.name).collect()
    }

    /// No selection means the whole server — and it has to keep meaning that
    /// rather than freezing today's list, or a server that gains a tool would
    /// never reach a session that asked for all of it.
    #[tokio::test]
    async fn an_unnarrowed_server_offers_every_tool() {
        let (tb, _db) = linear().await;
        assert_eq!(
            spec_names(&tb.with_allowed(None)),
            vec![
                "mcp__linear__search_issues",
                "mcp__linear__create_issue",
                "mcp__linear__delete_issue"
            ]
        );
    }

    #[tokio::test]
    async fn a_narrowed_server_offers_only_the_tools_it_names() {
        let (tb, _db) = linear().await;
        let tb = tb.with_allowed(Some(&["search_issues".to_string()]));
        assert_eq!(spec_names(&tb), vec!["mcp__linear__search_issues"]);
        // Still callable, so narrowing does not break the tool it kept.
        assert!(
            tb.execute("mcp__linear__search_issues", json!({}), "tc1")
                .await
                .is_ok()
        );
    }

    /// Hiding a tool from the spec list is not enough: a model that saw it on
    /// an earlier turn, or guessed the name, will call it anyway.
    #[tokio::test]
    async fn calling_a_tool_that_was_not_selected_is_refused_in_its_own_words() {
        let (tb, _db) = linear().await;
        let tb = tb.with_allowed(Some(&["search_issues".to_string()]));
        let Err(ToolCallError::InvalidInput(said)) = tb
            .execute("mcp__linear__delete_issue", json!({}), "tc1")
            .await
        else {
            panic!("an unselected tool must be refused");
        };
        assert!(said.contains("delete_issue"), "{said}");
        assert!(said.contains("not among the tools selected"), "{said}");
        // Not the same sentence as a name this server never had — those are
        // different situations and the model acts differently on each.
        let Err(ToolCallError::InvalidInput(unknown)) =
            tb.execute("mcp__other__thing", json!({}), "tc1").await
        else {
            panic!("a foreign name must be refused too");
        };
        assert!(unknown.contains("is not a tool of MCP server"), "{unknown}");
    }

    /// The stored catalogue describes the *server*. If narrowing reached it, a
    /// session that picked two tools would shrink the picker for everyone.
    #[tokio::test]
    async fn narrowing_does_not_shrink_the_catalogue_the_server_advertised() {
        let (tb, _db) = linear().await;
        let tb = tb.with_allowed(Some(&["search_issues".to_string()]));
        assert_eq!(tb.tool_defs().len(), 3);
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
            ToolOutcome::result(json!("ran beta"))
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
        let (sink, _db) = sink().await;
        let tb = McpToolbox::connect("github".into(), client, sink)
            .await
            .unwrap();

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
        assert_eq!(out, ToolOutcome::result(json!("PR #7 opened")));

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
            horsie_runtime_host::RuntimeClient::detached(
                horsie_runtime_host::testkit::MockTransport::ok(""),
                "agent",
            ),
            vec![horsie_models::runtime::PluginMcpTool {
                name: "mcp__github__open_pr".into(),
                description: Some("the plugin's".into()),
                input_schema: r#"{"type":"object"}"#.into(),
            }],
            None,
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
                sink().await.0,
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
            ToolOutcome::result(json!("from the admin server"))
        );
    }

    /// The reported bug, on the server-side path: a screenshot tool answers
    /// with an `image` block and the model used to be handed an empty string.
    /// The bytes are stored here and the outcome carries the reference.
    #[tokio::test]
    async fn an_image_block_becomes_a_stored_artifact_on_the_outcome() {
        let client = mock_client(vec![
            ("initialize", json!({})),
            (
                "tools/list",
                json!({ "tools": [ { "name": "screenshot", "inputSchema": { "type": "object" } } ] }),
            ),
            (
                "tools/call",
                json!({ "content": [
                    { "type": "text", "text": "here it is" },
                    { "type": "image", "data": b64(&png()), "mimeType": "image/png" }
                ] }),
            ),
        ]);
        let (sink, db) = sink().await;
        let service = sink.service.clone();
        let tb = McpToolbox::connect("shots".into(), client, sink)
            .await
            .unwrap();

        let ToolOutcome::Result(value) = tb
            .execute("mcp__shots__screenshot", json!({}), "tc1")
            .await
            .unwrap()
        else {
            panic!("a screenshot does not end the run");
        };
        assert_eq!(value.value, json!("here it is"));
        assert_eq!(value.artifacts.len(), 1);
        assert_eq!(value.artifacts[0].media_type, "image/png");
        // Stored, not merely described: the bytes come back out again.
        assert_eq!(
            service
                .get(&ProjectId::new("p1"), &value.artifacts[0].id)
                .await
                .unwrap(),
            png()
        );
        drop(db);
    }

    /// An artifact that will not store costs itself and nothing else. Losing a
    /// screenshot is much better than losing the whole result — so the text
    /// still arrives and the call still succeeds.
    #[tokio::test]
    async fn an_artifact_that_will_not_store_degrades_to_text() {
        let client = mock_client(vec![
            ("initialize", json!({})),
            (
                "tools/list",
                json!({ "tools": [ { "name": "screenshot", "inputSchema": { "type": "object" } } ] }),
            ),
            (
                "tools/call",
                json!({ "content": [
                    { "type": "text", "text": "here it is" },
                    // Claims to be a PNG and is not. The service sniffs the
                    // bytes and refuses them, which is the failure this covers.
                    { "type": "image", "data": b64(b"definitely not an image"),
                      "mimeType": "image/png" }
                ] }),
            ),
        ]);
        let (sink, _db) = sink().await;
        let tb = McpToolbox::connect("shots".into(), client, sink)
            .await
            .unwrap();

        let ToolOutcome::Result(value) = tb
            .execute("mcp__shots__screenshot", json!({}), "tc1")
            .await
            .unwrap()
        else {
            panic!("a failed artifact must not end the run");
        };
        assert_eq!(value.value, json!("here it is"));
        assert!(value.artifacts.is_empty());
    }

    /// The runtime ships bytes because it has no database; the server is what
    /// stores them. A plugin MCP screenshot has to survive that hop too.
    #[tokio::test]
    async fn a_plugin_mcp_result_stores_the_runtimes_bytes() {
        let client = horsie_runtime_host::RuntimeClient::detached(
            horsie_runtime_host::testkit::MockTransport::output(
                horsie_models::runtime::ToolOutput {
                    stdout: "here it is".into(),
                    stderr: String::new(),
                    exit_code: 0,
                    artifacts: vec![fluorite::Bytes(png())],
                },
            ),
            "agent",
        );
        let (sink, _db) = sink().await;
        let tb = PluginMcpToolbox::new(
            client,
            vec![horsie_models::runtime::PluginMcpTool {
                name: "mcp__shots__screenshot".into(),
                description: None,
                input_schema: r#"{"type":"object"}"#.into(),
            }],
            Some(sink),
        );
        let ToolOutcome::Result(value) = tb
            .execute("mcp__shots__screenshot", json!({}), "tc1")
            .await
            .unwrap()
        else {
            panic!("a screenshot does not end the run");
        };
        assert_eq!(value.value, json!("here it is"));
        assert_eq!(value.artifacts.len(), 1);
        assert_eq!(value.artifacts[0].media_type, "image/png");
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
        let (sink, _db) = sink().await;
        let tb = McpToolbox::connect("srv".into(), client, sink)
            .await
            .unwrap();
        match tb.execute("mcp__srv__boom", json!({}), "tc1").await {
            Err(ToolCallError::ExecutionFailed(msg)) => assert_eq!(msg, "kaboom"),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
