//! `horsie connect`: wraps the standalone `horsie-runtime --endpoint ...`
//! dial-back flow (see `docs/guide/getting-started.md`) so installing one
//! binary, `horsie`, is enough to connect a machine to a session server.

use crate::error::CliError;

/// Translate a `--server` URL (`http(s)://host[:port]`) into the
/// `ws(s)://.../api/runtime/connect?register=<runtime_id>` endpoint
/// `horsie-runtime` expects.
pub fn server_to_endpoint(server: &str, runtime_id: &str) -> Result<String, CliError> {
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| CliError::Validation(format!("--server must be a URL, got '{server}'")))?;
    let ws_scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(CliError::Validation(format!(
                "--server must be http:// or https://, got '{other}://'"
            )));
        }
    };
    let rest = rest.trim_end_matches('/');
    Ok(format!(
        "{ws_scheme}://{rest}/api/runtime/connect?register={runtime_id}"
    ))
}

/// A bare path (no `=`) becomes `main=<path>`; `name=path` passes through
/// unchanged. `horsie-runtime`'s own parser (`WorkspaceRegistry::parse_arg`)
/// requires `name=path`, so this is the only workspace-syntax leniency
/// `horsie connect` adds on top.
pub fn normalize_workspace_arg(s: &str) -> String {
    if s.contains('=') {
        s.to_string()
    } else {
        format!("main={s}")
    }
}

/// The one-line confirmation printed once `horsie-runtime` is launched.
/// `workspaces` are already-normalized `name=path` strings.
pub fn connection_summary(server: &str, runtime_id: &str, workspaces: &[String]) -> String {
    let list = workspaces
        .iter()
        .map(|w| {
            let (name, path) = w.split_once('=').unwrap_or(("main", w.as_str()));
            format!("workspace \"{name}\" -> {path}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("connected to {server} as runtime \"{runtime_id}\" · {list}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_to_endpoint_maps_http_to_ws() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789", "local").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_maps_https_to_wss() {
        assert_eq!(
            server_to_endpoint("https://horsie.example.com", "shawn-laptop").unwrap(),
            "wss://horsie.example.com/api/runtime/connect?register=shawn-laptop"
        );
    }

    #[test]
    fn server_to_endpoint_strips_trailing_slash() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789/", "local").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_rejects_non_http_scheme() {
        assert!(server_to_endpoint("ws://localhost:3789", "local").is_err());
        assert!(server_to_endpoint("localhost:3789", "local").is_err());
    }

    #[test]
    fn normalize_workspace_arg_defaults_bare_path_to_main() {
        assert_eq!(normalize_workspace_arg("."), "main=.");
        assert_eq!(normalize_workspace_arg("/home/shawn/proj"), "main=/home/shawn/proj");
    }

    #[test]
    fn normalize_workspace_arg_passes_through_name_eq_path() {
        assert_eq!(normalize_workspace_arg("api=./api"), "api=./api");
    }

    #[test]
    fn connection_summary_lists_every_workspace() {
        let summary = connection_summary(
            "http://localhost:3789",
            "local",
            &["main=.".to_string(), "api=./api".to_string()],
        );
        assert_eq!(
            summary,
            "connected to http://localhost:3789 as runtime \"local\" · \
             workspace \"main\" -> ., workspace \"api\" -> ./api"
        );
    }
}
