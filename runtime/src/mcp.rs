//! MCP servers the loaded plugins declare, hosted here because here is where
//! the plugin files are.
//!
//! A plugin's `.mcp.json` describes servers that belong next to the workspace: a
//! `npx …` one is a local process, and even a remote one is as likely to sit on
//! the workspace's network as on the public internet. Running them in the server
//! process would be wrong for the first and arbitrary for the second, so there
//! is one rule with no exceptions — **plugin MCP runs in the sandbox,
//! admin-configured MCP runs in the server**.
//!
//! Connections live as long as the runtime connection and are started on first
//! use. A stdio child respawned per tool call would cost more than the call.

use horsie_mcp_client::{McpClient, McpError, StdioTransport};
use horsie_models::runtime::PluginMcpTool;
use horsie_support::plugin::mcp::{McpTransportSpec, PluginMcpServer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Namespace one server's tool, the spelling admin-configured servers use.
fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Split a namespaced name back into `(server, tool)`.
///
/// A tool name may itself contain `__`, so the *server* is taken from the first
/// separator and the rest is the tool — the same split the namespacing implies.
fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Live connections, keyed by declared server name.
#[derive(Default)]
pub struct McpRegistry {
    clients: Mutex<BTreeMap<String, Arc<McpClient>>>,
}

/// What one discovery pass produced.
pub struct Discovery {
    pub tools: Vec<PluginMcpTool>,
    /// `<server>: <why>` for each server that could not be reached. Reported
    /// rather than dropped: a plugin bringing a broken server must not stop a
    /// session that merely loads it, but it must be visible.
    pub failures: Vec<String>,
}

impl McpRegistry {
    /// Every server declared by every installed plugin, with
    /// `${CLAUDE_PLUGIN_ROOT}` resolved against the plugin it came from.
    ///
    /// A plugin whose `.mcp.json` is malformed contributes nothing and is
    /// named, exactly as an unreadable manifest contributes no skills.
    pub fn declared(plugins_dir: &Path) -> (Vec<(PluginMcpServer, PathBuf)>, Vec<String>) {
        let mut out = Vec::new();
        let mut failures = Vec::new();
        for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
            match horsie_support::plugin::mcp::read(&plugin_root) {
                Ok(servers) => {
                    for server in servers {
                        out.push((expand_root(server, &plugin_root), plugin_root.clone()));
                    }
                }
                Err(e) => failures.push(format!("{}: {e}", plugin_root.display())),
            }
        }
        (out, failures)
    }

    /// Connect to every declared server and list its tools.
    ///
    /// `cwd` is where a stdio server runs — the first workspace, so a server
    /// that reads files reads the ones the agent is working on.
    pub async fn discover(&self, plugins_dir: &Path, cwd: Option<&Path>) -> Discovery {
        let (declared, mut failures) = Self::declared(plugins_dir);
        let mut tools = Vec::new();
        for (server, _) in declared {
            let client = match self.connect(&server, cwd).await {
                Ok(client) => client,
                Err(e) => {
                    tracing::warn!(server = %server.name, error = %e, "MCP server unavailable");
                    failures.push(format!("{}: {e}", server.name));
                    continue;
                }
            };
            match client.list_tools().await {
                Ok(listed) => tools.extend(listed.into_iter().map(|t| PluginMcpTool {
                    name: namespaced(&server.name, &t.name),
                    description: Some(t.description).filter(|d| !d.is_empty()),
                    input_schema: t.input_schema.to_string(),
                })),
                Err(e) => {
                    tracing::warn!(server = %server.name, error = %e, "MCP tools/list failed");
                    failures.push(format!("{}: {e}", server.name));
                }
            }
        }
        Discovery { tools, failures }
    }

    /// Call one namespaced tool.
    pub async fn invoke(
        &self,
        plugins_dir: &Path,
        cwd: Option<&Path>,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<String, String> {
        let Some((server_name, tool_name)) = split_namespaced(tool) else {
            return Err(format!("'{tool}' is not an MCP tool name"));
        };
        let (declared, _) = Self::declared(plugins_dir);
        let Some((server, _)) = declared.into_iter().find(|(s, _)| s.name == server_name) else {
            return Err(format!("no plugin declares an MCP server '{server_name}'"));
        };
        let client = self
            .connect(&server, cwd)
            .await
            .map_err(|e| format!("{server_name}: {e}"))?;
        let outcome = client
            .call_tool(tool_name, arguments)
            .await
            .map_err(|e| format!("{server_name}: {e}"))?;
        if outcome.is_error {
            return Err(outcome.text);
        }
        Ok(outcome.text)
    }

    /// The live client for a server, connecting and handshaking on first use.
    async fn connect(
        &self,
        server: &PluginMcpServer,
        cwd: Option<&Path>,
    ) -> Result<Arc<McpClient>, McpError> {
        // Held across the connect so two concurrent tool calls cannot each
        // spawn the same server — a duplicate stdio child would be a second
        // process nobody ever kills.
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&server.name) {
            return Ok(Arc::clone(client));
        }
        let client = Arc::new(McpClient::new(match &server.transport {
            McpTransportSpec::Stdio { command, args, env } => {
                Arc::new(StdioTransport::spawn(command, args, env, cwd).await?) as Arc<_>
            }
            McpTransportSpec::Http { url, headers } => {
                Arc::new(horsie_mcp_client::HttpTransport::new(
                    url.clone(),
                    Arc::new(StaticHeaders(headers.clone())),
                )) as Arc<_>
            }
        }));
        client.initialize().await?;
        clients.insert(server.name.clone(), Arc::clone(&client));
        Ok(client)
    }
}

/// A plugin's declared headers, as the bearer the HTTP transport asks for.
///
/// There is no OAuth here and that is deliberate: OAuth needs a redirect back to
/// *the server*, and a `.mcp.json` has nowhere to record a client registration.
/// What the format does have is a static `Authorization`, which is how a
/// published declaration carries a token.
struct StaticHeaders(Vec<(String, String)>);

#[async_trait::async_trait]
impl horsie_mcp_client::BearerProvider for StaticHeaders {
    async fn bearer(&self, _force: bool) -> Result<Option<String>, McpError> {
        Ok(self
            .0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.trim_start_matches("Bearer ").to_string()))
    }
}

/// Substitute `${CLAUDE_PLUGIN_ROOT}` everywhere a declaration can name a path.
///
/// A plugin shipping its own server script has no other way to say where it is,
/// and the root is only knowable here — which is why the reader leaves the
/// placeholder alone.
fn expand_root(server: PluginMcpServer, plugin_root: &Path) -> PluginMcpServer {
    let root = plugin_root.to_string_lossy();
    let sub = |s: &str| s.replace("${CLAUDE_PLUGIN_ROOT}", &root);
    let transport = match server.transport {
        McpTransportSpec::Stdio { command, args, env } => McpTransportSpec::Stdio {
            command: sub(&command),
            args: args.iter().map(|a| sub(a)).collect(),
            env: env.iter().map(|(k, v)| (k.clone(), sub(v))).collect(),
        },
        McpTransportSpec::Http { url, headers } => McpTransportSpec::Http {
            url: sub(&url),
            headers: headers.iter().map(|(k, v)| (k.clone(), sub(v))).collect(),
        },
    };
    PluginMcpServer {
        transport,
        ..server
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
    use tempfile::TempDir;

    fn declare(plugins: &Path, plugin: &str, json: &str) {
        let dir = plugins.join(plugin);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".mcp.json"), json).unwrap();
    }

    #[test]
    fn a_namespaced_name_round_trips() {
        assert_eq!(namespaced("docs", "search"), "mcp__docs__search");
        assert_eq!(
            split_namespaced("mcp__docs__search"),
            Some(("docs", "search"))
        );
        // A tool whose own name contains the separator still splits at the
        // server boundary, which is the first one.
        assert_eq!(
            split_namespaced("mcp__docs__deep__search"),
            Some(("docs", "deep__search"))
        );
        assert!(split_namespaced("bash").is_none());
        assert!(split_namespaced("mcp__docs").is_none());
    }

    #[test]
    fn declared_collects_every_plugins_servers() {
        let plugins = TempDir::new().unwrap();
        declare(
            plugins.path(),
            "a",
            r#"{"mcpServers":{"one":{"command":"x"}}}"#,
        );
        declare(plugins.path(), "b", r#"{"two":{"url":"https://y"}}"#);
        // No `.mcp.json` at all: contributes nothing, silently.
        std::fs::create_dir_all(plugins.path().join("c")).unwrap();
        let (servers, failures) = McpRegistry::declared(plugins.path());
        let mut names: Vec<&str> = servers.iter().map(|(s, _)| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["one", "two"]);
        assert!(failures.is_empty());
    }

    /// One plugin's malformed declaration must not blank the library.
    #[test]
    fn a_malformed_declaration_is_named_not_fatal() {
        let plugins = TempDir::new().unwrap();
        declare(plugins.path(), "bad", "{not json");
        declare(
            plugins.path(),
            "good",
            r#"{"mcpServers":{"ok":{"command":"x"}}}"#,
        );
        let (servers, failures) = McpRegistry::declared(plugins.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("bad"), "{failures:?}");
    }

    /// The root is only knowable here, which is why the reader leaves the
    /// placeholder for this step.
    #[test]
    fn the_plugin_root_is_substituted_against_the_plugin_that_declared_it() {
        let plugins = TempDir::new().unwrap();
        declare(
            plugins.path(),
            "own",
            r#"{"mcpServers":{"s":{"command":"node",
                 "args":["${CLAUDE_PLUGIN_ROOT}/server.js"],
                 "env":{"ROOT":"${CLAUDE_PLUGIN_ROOT}"}}}}"#,
        );
        let (servers, _) = McpRegistry::declared(plugins.path());
        match &servers[0].0.transport {
            McpTransportSpec::Stdio { args, env, .. } => {
                let expected = plugins.path().join("own").display().to_string();
                assert_eq!(args[0], format!("{expected}/server.js"));
                assert_eq!(env[0].1, expected);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A server that cannot start contributes no tools and is reported — a
    /// session that merely loads the plugin still runs.
    #[tokio::test]
    async fn a_server_that_cannot_start_is_a_failure_not_a_panic() {
        let plugins = TempDir::new().unwrap();
        declare(
            plugins.path(),
            "broken",
            r#"{"mcpServers":{"nope":{"command":"horsie-no-such-binary"}}}"#,
        );
        let discovery = McpRegistry::default().discover(plugins.path(), None).await;
        assert!(discovery.tools.is_empty());
        assert_eq!(discovery.failures.len(), 1);
        assert!(
            discovery.failures[0].contains("nope"),
            "{:?}",
            discovery.failures
        );
    }

    /// End to end against a real child: handshake, list, and the namespacing
    /// the server sees applied to what it returned.
    #[tokio::test]
    async fn discovery_namespaces_a_live_servers_tools() {
        let plugins = TempDir::new().unwrap();
        let script = r#"while read -r line; do
             id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
             case "$line" in
               *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search","description":"finds","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
               *initialize*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
               *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
             esac
           done"#;
        let json = serde_json::json!({
            "mcpServers": { "docs": { "command": "sh", "args": ["-c", script] } }
        });
        declare(plugins.path(), "p", &json.to_string());
        let discovery = McpRegistry::default().discover(plugins.path(), None).await;
        assert!(discovery.failures.is_empty(), "{:?}", discovery.failures);
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.tools[0].name, "mcp__docs__search");
        assert_eq!(discovery.tools[0].description.as_deref(), Some("finds"));
    }
}
