//! `horsie connect`: wraps the standalone `horsie-runtime --endpoint ...`
//! dial-back flow (see `docs/guide/getting-started.md`) so installing one
//! binary, `horsie`, is enough to connect a machine to a session server.

use crate::error::CliError;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Spawn `horsie-runtime` to dial `server` as this machine's runtime.
/// Foreground by default — the child inherits this process's stdio, so its
/// errors surface directly and the parent blocks until it exits or is
/// interrupted. `background` detaches it instead, with output redirected to
/// `<state_dir>/connect.log`.
pub fn run(
    runtime_bin: &Path,
    server: &str,
    workspaces: &[String],
    runtime_id: &str,
    background: bool,
    state_dir: &Path,
) -> Result<i32, CliError> {
    let endpoint = server_to_endpoint(server, runtime_id)?;
    let normalized: Vec<String> = workspaces
        .iter()
        .map(|w| normalize_workspace_arg(w))
        .collect();

    let mut cmd = Command::new(runtime_bin);
    cmd.arg("--endpoint")
        .arg(&endpoint)
        .arg("--runtime-id")
        .arg(runtime_id);
    for w in &normalized {
        cmd.arg("--workspace").arg(w);
    }

    println!("{}", connection_summary(server, runtime_id, &normalized));
    println!("open {server} in your browser to start a session");

    if background {
        std::fs::create_dir_all(state_dir).map_err(|e| CliError::Io(e.to_string()))?;
        let log_path = state_dir.join("connect.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| CliError::Io(e.to_string()))?;
        let err_log = log.try_clone().map_err(|e| CliError::Io(e.to_string()))?;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log));
        let child = cmd.spawn().map_err(|e| spawn_error(runtime_bin, &e))?;
        println!(
            "running in background (pid {}, log at {})",
            child.id(),
            log_path.display()
        );
        Ok(0)
    } else {
        let status = cmd.status().map_err(|e| spawn_error(runtime_bin, &e))?;
        Ok(status.code().unwrap_or(1))
    }
}

fn spawn_error(runtime_bin: &Path, e: &std::io::Error) -> CliError {
    CliError::Executor(format!(
        "failed to launch horsie-runtime at {} ({e}); reinstall the CLI so \
         horsie-runtime is installed alongside horsie",
        runtime_bin.display()
    ))
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
        assert_eq!(
            normalize_workspace_arg("/home/shawn/proj"),
            "main=/home/shawn/proj"
        );
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
