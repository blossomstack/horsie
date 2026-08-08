//! `<plugin>/.mcp.json` — the MCP servers a plugin brings with it.
//!
//! A **top-level file**, not a `plugin.json` field, which is why it is read here
//! rather than in [`super::manifest`].
//!
//! `${CLAUDE_PLUGIN_ROOT}` is deliberately left unsubstituted: the root that
//! matters is the path the *runtime* has the plugin at, and this reader may be
//! running somewhere else entirely.

use std::path::Path;

/// How one declared server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransportSpec {
    /// A local process speaking JSON-RPC on stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        /// Extra environment, in declaration order.
        env: Vec<(String, String)>,
    },
    /// A remote endpoint speaking Streamable HTTP.
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// One MCP server a plugin declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMcpServer {
    /// The key it was declared under. Namespaces its tools as
    /// `mcp__<name>__<tool>`.
    pub name: String,
    pub transport: McpTransportSpec,
}

/// Read `<plugin_root>/.mcp.json`.
///
/// An absent file is an empty set. A present but malformed one is an error, for
/// the reason a malformed `hooks.json` is: silently reading it as "no servers"
/// is the failure this whole feature exists to remove.
pub fn read(plugin_root: &Path) -> Result<Vec<PluginMcpServer>, String> {
    let path = plugin_root.join(".mcp.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!(".mcp.json: {e}"))?;
    Ok(parse(&json))
}

/// Both shapes in the wild: servers wrapped in `mcpServers`, or at the top
/// level — which is what the official `example-plugin` ships. One branch, and
/// it is what the ecosystem actually contains.
fn parse(json: &serde_json::Value) -> Vec<PluginMcpServer> {
    let map = json
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .or_else(|| json.as_object());
    let Some(map) = map else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, spec)| {
            // A declaration naming neither a command nor a url is unreachable.
            // Skipped silently here, as an unrunnable hook declaration is: this
            // crate reads the format, and the runtime that uses the result is
            // where a warning belongs.
            let transport = transport_of(spec)?;
            Some(PluginMcpServer {
                name: name.clone(),
                transport,
            })
        })
        .collect()
}

/// `type` when declared, else inferred: a `command` is stdio, a `url` is http.
/// Inference matters — most published declarations omit `type` entirely.
fn transport_of(spec: &serde_json::Value) -> Option<McpTransportSpec> {
    let str_at = |k: &str| spec.get(k).and_then(serde_json::Value::as_str);
    let pairs = |k: &str| -> Vec<(String, String)> {
        spec.get(k)
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let declared = str_at("type");
    match (declared, str_at("command"), str_at("url")) {
        (Some("stdio"), Some(command), _) | (None, Some(command), None) => {
            Some(McpTransportSpec::Stdio {
                command: command.to_string(),
                args: spec
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| Some(v.as_str()?.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                env: pairs("env"),
            })
        }
        // `sse` is the older spelling of the same remote shape; both reach a URL.
        (Some("http" | "sse"), _, Some(url)) | (None, None, Some(url)) => {
            Some(McpTransportSpec::Http {
                url: url.to_string(),
                headers: pairs("headers"),
            })
        }
        _ => None,
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

    fn write(root: &Path, json: &str) {
        std::fs::write(root.join(".mcp.json"), json).unwrap();
    }

    #[test]
    fn an_absent_file_is_an_empty_set_not_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(read(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "{not json");
        assert!(read(dir.path()).unwrap_err().contains(".mcp.json"));
    }

    /// The wrapped shape, with the transport inferred from `command` — which is
    /// how the overwhelming majority of published declarations are written.
    #[test]
    fn reads_a_wrapped_stdio_server_with_the_type_inferred() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"mcpServers":{"docs":{"command":"npx","args":["-y","@acme/docs"],
                 "env":{"KEY":"abc"}}}}"#,
        );
        let servers = read(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(
            servers[0].transport,
            McpTransportSpec::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@acme/docs".into()],
                env: vec![("KEY".into(), "abc".into())],
            }
        );
    }

    /// The official `example-plugin`'s shape, verbatim: no `mcpServers`
    /// wrapper, and an explicit `type: "http"`.
    #[test]
    fn reads_the_unwrapped_shape_the_official_example_ships() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"example-server":{"type":"http","url":"https://mcp.example.com/api"}}"#,
        );
        let servers = read(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "example-server");
        assert_eq!(
            servers[0].transport,
            McpTransportSpec::Http {
                url: "https://mcp.example.com/api".into(),
                headers: Vec::new(),
            }
        );
    }

    /// `sse` is the older spelling of the same remote shape.
    #[test]
    fn sse_is_read_as_the_remote_shape_it_is() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"mcpServers":{"old":{"type":"sse","url":"https://x/sse"}}}"#,
        );
        assert!(matches!(
            read(dir.path()).unwrap()[0].transport,
            McpTransportSpec::Http { .. }
        ));
    }

    /// A declaration naming neither a command nor a url is unreachable, so it
    /// is skipped rather than half-registered.
    #[test]
    fn a_server_with_no_way_to_reach_it_is_skipped() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"mcpServers":{"broken":{"type":"stdio"},"fine":{"command":"x"}}}"#,
        );
        let servers = read(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fine");
    }

    /// The root is the *runtime's* path, so the placeholder survives this
    /// reader untouched.
    #[test]
    fn the_plugin_root_placeholder_is_left_for_the_runtime() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"mcpServers":{"own":{"command":"node",
                 "args":["${CLAUDE_PLUGIN_ROOT}/server.js"]}}}"#,
        );
        match &read(dir.path()).unwrap()[0].transport {
            McpTransportSpec::Stdio { args, .. } => {
                assert_eq!(args[0], "${CLAUDE_PLUGIN_ROOT}/server.js");
            }
            other => panic!("{other:?}"),
        }
    }
}
