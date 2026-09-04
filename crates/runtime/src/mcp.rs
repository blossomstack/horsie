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

use horsie_models::runtime::{
    McpServerFailure, McpServerNeedsAuth, McpServerUnreachable, PluginMcpTool, ToolOutput,
};
use horsie_support::mcp::{HttpTransport, McpClient, McpError, McpImage, StdioTransport};
use horsie_support::plugin::mcp::{McpTransportSpec, PluginMcpServer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

/// What discovery will wait for one server before giving up on it *for this
/// turn*. `provide()` is on the turn's critical path, so a server that is slow
/// to start costs the user a turn's tools, never the turn.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(20);

/// Mirrors the hook and bash clamps. An MCP server is as capable of returning a
/// megabyte as a command is, and the transcript pays for it either way.
///
/// Text only. Artifact bytes are not transcript and are bounded separately, by
/// [`MAX_ARTIFACTS`] and [`MAX_ARTIFACT_BYTES`] — clamping an image at 50 KB
/// would merely corrupt it.
const OUTPUT_CLAMP: usize = 50_000;

/// The most artifacts one MCP tool result may carry.
///
/// A screenshot tool answers with one image, and a busy one with a handful.
/// Past that a result is a data dump rather than an answer, and an unbounded
/// one would pin arbitrary memory here *and* in the server it is sent to.
const MAX_ARTIFACTS: usize = 8;

/// The most artifact bytes one MCP tool result may carry, in total.
///
/// 10 MB, which is what the server will store for a single artifact
/// (`ArtifactService::MAX_ARTIFACT_BYTES`). Carrying more than the far end can
/// keep buys nothing and costs a copy in two processes.
const MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;

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

/// Truncate on a char boundary, saying so.
fn clamp(text: String) -> String {
    if text.len() <= OUTPUT_CLAMP {
        return text;
    }
    let mut cut = OUTPUT_CLAMP;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… truncated at {OUTPUT_CLAMP} bytes", &text[..cut])
}

/// Take the images that fit within the caps, in the order the server returned
/// them, and say how many were left behind.
///
/// An outsized block costs itself rather than everything behind it, so one
/// enormous screenshot does not swallow the small ones that followed. The
/// dropped count comes back because the caller has to *say* so: an artifact
/// that vanished without a word is worse than one that never existed.
fn within_caps(images: Vec<McpImage>) -> (Vec<Vec<u8>>, usize) {
    let total = images.len();
    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut budget = MAX_ARTIFACT_BYTES;
    for McpImage(bytes) in images {
        if kept.len() == MAX_ARTIFACTS {
            break;
        }
        if bytes.len() > budget {
            continue;
        }
        budget -= bytes.len();
        kept.push(bytes);
    }
    let dropped = total - kept.len();
    (kept, dropped)
}

fn unreachable(server: &str, reason: impl std::fmt::Display) -> McpServerFailure {
    McpServerFailure::Unreachable(McpServerUnreachable {
        server: server.to_string(),
        reason: reason.to_string(),
    })
}

/// One declared server and the plugin that declared it. The root is kept
/// because both `${CLAUDE_PLUGIN_ROOT}` and the child's own
/// `CLAUDE_PLUGIN_ROOT` need it.
pub struct DeclaredServer {
    pub server: PluginMcpServer,
    pub plugin_root: PathBuf,
}

/// Live connections, keyed by declared server name.
///
/// The value is a `OnceCell` rather than the client itself so the map lock is
/// held only long enough to claim a slot: a handshake that spawns `npx` can
/// take minutes, and holding the map across it would serialise every other
/// server behind the slowest one.
/// Keyed by `(agent_id, server_name)`, not by server name alone. Two agents can
/// select different bundles that declare the same server name, and a cache keyed
/// on the name would hand the second agent the first agent's process — running
/// out of the wrong plugin root, with the wrong environment.
/// One agent's connection to one named server, claimed before it is built —
/// see the `OnceCell` note above.
type ClientSlot = Arc<OnceCell<Arc<McpClient>>>;

#[derive(Default)]
pub struct McpRegistry {
    clients: Mutex<BTreeMap<(String, String), ClientSlot>>,
}

/// What one discovery pass produced.
pub struct Discovery {
    pub tools: Vec<PluginMcpTool>,
    /// Every server that contributed no tools, and why. Reported rather than
    /// dropped: a plugin bringing a broken server must not stop a session that
    /// merely loads it, but it must be visible.
    pub failures: Vec<McpServerFailure>,
}

impl McpRegistry {
    /// Every server declared by every installed plugin, with
    /// `${CLAUDE_PLUGIN_ROOT}` resolved against the plugin it came from.
    ///
    /// A plugin whose `.mcp.json` is malformed contributes nothing and is
    /// named, exactly as an unreadable manifest contributes no skills.
    pub fn declared(plugins_dir: &Path) -> (Vec<DeclaredServer>, Vec<McpServerFailure>) {
        let mut out: Vec<DeclaredServer> = Vec::new();
        let mut failures = Vec::new();
        for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
            match horsie_support::plugin::mcp::read(&plugin_root) {
                Ok(servers) => {
                    for server in servers {
                        // First declaration wins, and the loser is named.
                        // Merging silently would hand one plugin another's tool
                        // namespace: the second server would never start, and
                        // the first's tools would be advertised twice.
                        if let Some(prior) = out.iter().find(|d| d.server.name == server.name) {
                            failures.push(unreachable(
                                &server.name,
                                format!("already declared by {}", prior.plugin_root.display()),
                            ));
                            continue;
                        }
                        out.push(DeclaredServer {
                            server: expand_root(server, &plugin_root),
                            plugin_root: plugin_root.clone(),
                        });
                    }
                }
                Err(e) => failures.push(unreachable(&plugin_root.display().to_string(), e)),
            }
        }
        (out, failures)
    }

    /// Connect to every declared server and list its tools.
    ///
    /// Servers are reached concurrently: they are independent, and one that is
    /// slow to start must not add its wait to every other server's.
    ///
    /// `cwd` is where a stdio server runs — the first workspace, so a server
    /// that reads files reads the ones the agent is working on.
    pub async fn discover(
        &self,
        agent_id: &str,
        plugins_dir: &Path,
        cwd: Option<&Path>,
    ) -> Discovery {
        let (declared, mut failures) = Self::declared(plugins_dir);
        let passes = declared.iter().map(|d| self.list_one(agent_id, d, cwd));
        // Collected in declaration order rather than completion order, so the
        // tool list a session sees does not depend on which server happened to
        // be quickest today.
        let mut tools = Vec::new();
        for outcome in futures_util::future::join_all(passes).await {
            match outcome {
                Ok(listed) => tools.extend(listed),
                Err(failure) => failures.push(failure),
            }
        }
        Discovery { tools, failures }
    }

    /// Connect to one server and list its tools, within the discovery budget.
    async fn list_one(
        &self,
        agent_id: &str,
        declared: &DeclaredServer,
        cwd: Option<&Path>,
    ) -> Result<Vec<PluginMcpTool>, McpServerFailure> {
        let name = &declared.server.name;
        let pass = async {
            let client = self.connect(agent_id, declared, cwd).await.map_err(|e| {
                tracing::warn!(server = %name, error = %e, "MCP server unavailable");
                as_failure(name, &e)
            })?;
            client.list_tools().await.map_err(|e| {
                tracing::warn!(server = %name, error = %e, "MCP tools/list failed");
                as_failure(name, &e)
            })
        };
        match tokio::time::timeout(DISCOVER_TIMEOUT, pass).await {
            Ok(Ok(listed)) => Ok(listed
                .into_iter()
                .map(|t| PluginMcpTool {
                    name: namespaced(name, &t.name),
                    description: Some(t.description).filter(|d| !d.is_empty()),
                    input_schema: t.input_schema.to_string(),
                })
                .collect()),
            Ok(Err(failure)) => Err(failure),
            Err(_) => Err(unreachable(
                name,
                format!(
                    "did not answer within {}s; if it installs on first run, its tools appear once it has",
                    DISCOVER_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    /// Call one namespaced tool.
    ///
    /// Answers with the whole [`ToolOutput`] rather than its text, because an
    /// MCP result is not only text: a screenshot tool's entire answer is bytes,
    /// and a caller handed a `String` could only have thrown them away.
    pub async fn invoke(
        &self,
        agent_id: &str,
        plugins_dir: &Path,
        cwd: Option<&Path>,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, String> {
        let Some((server_name, tool_name)) = split_namespaced(tool) else {
            return Err(format!("'{tool}' is not an MCP tool name"));
        };
        let (declared, _) = Self::declared(plugins_dir);
        let Some(declared) = declared.into_iter().find(|d| d.server.name == server_name) else {
            return Err(format!("no plugin declares an MCP server '{server_name}'"));
        };
        let client = self
            .connect(agent_id, &declared, cwd)
            .await
            .map_err(|e| format!("{server_name}: {e}"))?;
        let outcome = client
            .call_tool(tool_name, arguments)
            .await
            .map_err(|e| format!("{server_name}: {e}"))?;
        if outcome.is_error {
            return Err(clamp(outcome.text));
        }
        let original_text_bytes = u64::try_from(outcome.text.len()).unwrap_or(u64::MAX);
        let (artifacts, dropped) = within_caps(outcome.images);
        let mut stdout = clamp(outcome.text);
        if dropped > 0 {
            stdout.push_str(&format!(
                "\n… {dropped} artifact(s) dropped: a tool result may carry at most \
                 {MAX_ARTIFACTS} artifacts and {MAX_ARTIFACT_BYTES} bytes"
            ));
        }
        let original_output_bytes =
            original_text_bytes.max(u64::try_from(stdout.len()).unwrap_or(u64::MAX));
        Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: artifacts.into_iter().map(fluorite::Bytes).collect(),
            original_output_bytes,
            spilled_output_bytes: 0,
        })
    }

    /// The live client for a server, connecting and handshaking on first use.
    ///
    /// The map lock is released before the handshake, so a server that takes an
    /// `npx` install to start blocks only callers of *that* server.
    async fn connect(
        &self,
        agent_id: &str,
        declared: &DeclaredServer,
        cwd: Option<&Path>,
    ) -> Result<Arc<McpClient>, McpError> {
        let cell = {
            let mut clients = self.clients.lock().await;
            Arc::clone(
                clients
                    .entry((agent_id.to_string(), declared.server.name.clone()))
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        cell.get_or_try_init(|| async {
            let client = Arc::new(McpClient::new(match &declared.server.transport {
                McpTransportSpec::Stdio { command, args, env } => Arc::new(
                    StdioTransport::spawn(command, args, env, cwd, Some(&declared.plugin_root))
                        .await?,
                ) as Arc<_>,
                // No OAuth here and that is deliberate: OAuth needs a redirect
                // back to *the server*, and a `.mcp.json` has nowhere to record
                // a client registration. What the format has is `headers`, and
                // they go on the wire exactly as declared.
                McpTransportSpec::Http { url, headers } => Arc::new(HttpTransport::with_headers(
                    url.clone(),
                    Arc::new(NoBearer),
                    headers.clone(),
                )) as Arc<_>,
            }));
            client.initialize().await?;
            Ok(client)
        })
        .await
        .cloned()
    }
}

/// A `401` is the one failure the server can act on, so it keeps its shape all
/// the way up. Everything else is a broken plugin to report.
fn as_failure(server: &str, error: &McpError) -> McpServerFailure {
    match error {
        McpError::Unauthorized { www_authenticate } => {
            McpServerFailure::NeedsAuth(McpServerNeedsAuth {
                server: server.to_string(),
                resource_metadata: www_authenticate
                    .as_deref()
                    .and_then(resource_metadata_of)
                    .map(str::to_string),
            })
        }
        e @ (McpError::Transport(_) | McpError::Protocol(_) | McpError::Rpc { .. }) => {
            unreachable(server, e)
        }
    }
}

/// The `resource_metadata="…"` parameter of an RFC 9728 `WWW-Authenticate`
/// challenge, which is what names the authorization server to talk to.
fn resource_metadata_of(challenge: &str) -> Option<&str> {
    let after = challenge.split("resource_metadata=").nth(1)?;
    let unquoted = after.strip_prefix('"')?;
    unquoted.split('"').next().filter(|s| !s.is_empty())
}

/// A plugin-declared server authenticates with what its declaration carries, so
/// there is no bearer to resolve.
struct NoBearer;

#[async_trait::async_trait]
impl horsie_support::mcp::BearerProvider for NoBearer {
    async fn bearer(&self, _force: bool) -> Result<Option<String>, McpError> {
        Ok(None)
    }
}

/// Substitute `${CLAUDE_PLUGIN_ROOT}` everywhere a declaration can name a path.
///
/// A plugin shipping its own server script has no other way to say where it is,
/// and the root is only knowable here — which is why the reader leaves the
/// placeholder alone.
fn expand_root(server: PluginMcpServer, plugin_root: &Path) -> PluginMcpServer {
    let sub = |s: &str| horsie_support::plugin::expand_plugin_root(s, plugin_root);
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

    fn reasons(failures: &[McpServerFailure]) -> Vec<(String, String)> {
        failures
            .iter()
            .map(|f| match f {
                McpServerFailure::Unreachable(u) => (u.server.clone(), u.reason.clone()),
                McpServerFailure::NeedsAuth(a) => (a.server.clone(), "needs auth".to_string()),
            })
            .collect()
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
        let mut names: Vec<&str> = servers.iter().map(|d| d.server.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["one", "two"]);
        assert!(failures.is_empty());
    }

    /// Two plugins naming the same server is a conflict, not a silent merge:
    /// the second's tools would land under names the first already owns, and
    /// its server would never start.
    #[test]
    fn a_duplicate_server_name_is_refused_rather_than_shadowed() {
        let plugins = TempDir::new().unwrap();
        declare(
            plugins.path(),
            "a",
            r#"{"mcpServers":{"docs":{"command":"x"}}}"#,
        );
        declare(
            plugins.path(),
            "b",
            r#"{"mcpServers":{"docs":{"command":"y"}}}"#,
        );
        let (servers, failures) = McpRegistry::declared(plugins.path());
        assert_eq!(servers.len(), 1, "the first declaration wins");
        assert_eq!(reasons(&failures).len(), 1);
        let (server, reason) = &reasons(&failures)[0];
        assert_eq!(server, "docs");
        assert!(reason.contains("already declared"), "{reason}");
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
        assert!(reasons(&failures)[0].0.contains("bad"), "{failures:?}");
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
        match &servers[0].server.transport {
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
        let discovery = McpRegistry::default()
            .discover("a1", plugins.path(), None)
            .await;
        assert!(discovery.tools.is_empty());
        assert_eq!(discovery.failures.len(), 1);
        assert_eq!(reasons(&discovery.failures)[0].0, "nope");
    }

    /// End to end against a real child: handshake, list, and the namespacing
    /// the server sees applied to what it returned.
    #[tokio::test]
    async fn discovery_namespaces_a_live_servers_tools() {
        let plugins = TempDir::new().unwrap();
        let json = serde_json::json!({
            "mcpServers": { "docs": { "command": "sh", "args": ["-c", LISTING_SERVER] } }
        });
        declare(plugins.path(), "p", &json.to_string());
        let discovery = McpRegistry::default()
            .discover("a1", plugins.path(), None)
            .await;
        assert!(discovery.failures.is_empty(), "{:?}", discovery.failures);
        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.tools[0].name, "mcp__docs__search");
        assert_eq!(discovery.tools[0].description.as_deref(), Some("finds"));
    }

    /// A server answering `401` is a consent problem, not a broken plugin, and
    /// says so — the server needs the distinction to offer a Connect flow.
    #[test]
    fn a_401_becomes_needs_auth_carrying_its_metadata_url() {
        let err = McpError::Unauthorized {
            www_authenticate: Some(
                r#"Bearer resource_metadata="https://x/.well-known/oauth-protected-resource""#
                    .to_string(),
            ),
        };
        match as_failure("remote", &err) {
            McpServerFailure::NeedsAuth(a) => {
                assert_eq!(a.server, "remote");
                assert_eq!(
                    a.resource_metadata.as_deref(),
                    Some("https://x/.well-known/oauth-protected-resource")
                );
            }
            other => panic!("{other:?}"),
        }
        // A challenge without the parameter still needs auth; the server falls
        // back to probing the endpoint's well-known path.
        let bare = McpError::Unauthorized {
            www_authenticate: Some("Bearer".to_string()),
        };
        match as_failure("remote", &bare) {
            McpServerFailure::NeedsAuth(a) => assert!(a.resource_metadata.is_none()),
            other => panic!("{other:?}"),
        }
    }

    /// Slow servers are independent, so their waits overlap. Three that never
    /// answer cost one budget, not three.
    #[tokio::test]
    async fn discovery_does_not_serialise_slow_servers() {
        let plugins = TempDir::new().unwrap();
        for name in ["a", "b", "c"] {
            let json = serde_json::json!({
                "mcpServers": { name: { "command": "sh", "args": ["-c", "sleep 30"] } }
            });
            declare(plugins.path(), name, &json.to_string());
        }
        let started = std::time::Instant::now();
        let discovery = tokio::time::timeout(
            DISCOVER_TIMEOUT + Duration::from_secs(10),
            McpRegistry::default().discover("a1", plugins.path(), None),
        )
        .await
        .expect("discovery must not outlive one budget");
        assert_eq!(discovery.failures.len(), 3);
        assert!(
            started.elapsed() < DISCOVER_TIMEOUT * 2,
            "three servers ran serially: {:?}",
            started.elapsed()
        );
    }

    /// The reported bug: a screenshot tool's whole answer is an `image` block,
    /// and it used to reach the model as an empty string.
    #[tokio::test]
    async fn an_image_block_reaches_the_tool_output_as_an_artifact() {
        let plugins = TempDir::new().unwrap();
        let json = serde_json::json!({
            "mcpServers": { "shots": { "command": "sh", "args": ["-c", SNAPPING_SERVER] } }
        });
        declare(plugins.path(), "p", &json.to_string());
        let out = McpRegistry::default()
            .invoke(
                "a1",
                plugins.path(),
                None,
                "mcp__shots__screenshot",
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "here it is");
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].as_ref(), b"hi");
    }

    /// A result carrying more artifacts than the cap allows keeps the cap's
    /// worth and *says* what it dropped — a screenshot that vanished without a
    /// word is worse than one that never existed.
    #[tokio::test]
    async fn a_flood_of_artifacts_is_capped_and_reported() {
        let plugins = TempDir::new().unwrap();
        let json = serde_json::json!({
            "mcpServers": { "flood": { "command": "sh", "args": ["-c", FLOODING_SERVER] } }
        });
        declare(plugins.path(), "p", &json.to_string());
        let out = McpRegistry::default()
            .invoke(
                "a1",
                plugins.path(),
                None,
                "mcp__flood__anything",
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.artifacts.len(), MAX_ARTIFACTS);
        assert!(
            out.stdout.contains("12 artifact(s) dropped"),
            "{}",
            out.stdout
        );
    }

    /// The byte budget is spent in order, and an outsized block costs itself
    /// rather than everything queued behind it.
    #[test]
    fn the_byte_budget_drops_what_will_not_fit_and_keeps_what_will() {
        let huge = McpImage(vec![0u8; MAX_ARTIFACT_BYTES + 1]);
        let small = McpImage(b"ok".to_vec());
        let (kept, dropped) = within_caps(vec![huge, small.clone()]);
        assert_eq!(kept, vec![b"ok".to_vec()]);
        assert_eq!(dropped, 1);

        // Two halves fit with room to spare; the third does not, and only it
        // is lost — the two-byte block behind it still gets through.
        let half = McpImage(vec![0u8; MAX_ARTIFACT_BYTES / 2 - 8]);
        let (kept, dropped) = within_caps(vec![half.clone(), half.clone(), half, small]);
        assert_eq!(kept.len(), 3, "the two halves and the two-byte block");
        assert_eq!(dropped, 1);
    }

    /// The 50 KB text clamp is a transcript rule, not a byte rule: an image is
    /// not truncated to fit it.
    #[test]
    fn the_text_clamp_does_not_apply_to_artifact_bytes() {
        let big = vec![0u8; OUTPUT_CLAMP * 3];
        let (kept, dropped) = within_caps(vec![McpImage(big.clone())]);
        assert_eq!(dropped, 0);
        assert_eq!(kept, vec![big]);
    }

    /// A server returning a megabyte must not put a megabyte in the transcript.
    #[tokio::test]
    async fn an_oversized_tool_result_is_clamped() {
        let plugins = TempDir::new().unwrap();
        let json = serde_json::json!({
            "mcpServers": { "big": { "command": "sh", "args": ["-c", SHOUTING_SERVER] } }
        });
        declare(plugins.path(), "p", &json.to_string());
        let out = McpRegistry::default()
            .invoke(
                "a1",
                plugins.path(),
                None,
                "mcp__big__anything",
                serde_json::json!({}),
            )
            .await
            .unwrap()
            .stdout;
        assert!(out.len() < OUTPUT_CLAMP + 200, "{} bytes", out.len());
        assert!(out.contains("truncated"), "{}", &out[out.len() - 60..]);
    }

    /// Answers `initialize` and lists one tool.
    const LISTING_SERVER: &str = r#"while read -r line; do
         id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
         case "$line" in
           *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search","description":"finds","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
           *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
         esac
       done"#;

    /// Answers every `tools/call` with a line of text and one image block —
    /// `aGk=` is "hi".
    const SNAPPING_SERVER: &str = r#"while read -r line; do
         id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
         case "$line" in
           *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"here it is"},{"type":"image","data":"aGk=","mimeType":"image/png"}]}}\n' "$id" ;;
           *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
         esac
       done"#;

    /// Answers every `tools/call` with 20 image blocks, well past the cap.
    const FLOODING_SERVER: &str = r#"blocks='{"type":"image","data":"aGk=","mimeType":"image/png"}'
       i=1
       while [ $i -lt 20 ]; do
         blocks="$blocks,"'{"type":"image","data":"aGk=","mimeType":"image/png"}'
         i=$((i+1))
       done
       while read -r line; do
         id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
         case "$line" in
           *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[%s]}}\n' "$id" "$blocks" ;;
           *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
         esac
       done"#;

    /// Answers every `tools/call` with far more text than anyone wants.
    const SHOUTING_SERVER: &str = r#"big=$(head -c 200000 /dev/zero | tr '\0' 'x')
       while read -r line; do
         id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
         case "$line" in
           *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$big" ;;
           *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
         esac
       done"#;
}
