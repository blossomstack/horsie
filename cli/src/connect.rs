//! `horsie connect`: wraps the standalone `horsie-runtime --endpoint ...`
//! dial-back flow (see `docs/guide/getting-started.md`) so installing one
//! binary, `horsie`, is enough to connect a machine to a session server.

use crate::error::CliError;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Translate a `--server` URL (`http(s)://host[:port]`) into the
/// `ws(s)://.../api/runtime/connect?register=local` endpoint
/// `horsie-runtime` expects. `register` is the server's vendor-kind
/// discriminator — only the literal `local` fires the local-daemon
/// registration hook; the runtime's own identity travels separately via
/// `--runtime-id` (announced as `RuntimeReady.runtime_id`).
pub fn server_to_endpoint(server: &str) -> Result<String, CliError> {
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
        "{ws_scheme}://{rest}/api/runtime/connect?register=local"
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

/// The argv for the spawned `horsie-runtime`, factored out of `run` so the
/// plugins wiring is unit-testable. `workspaces` must already be normalized
/// (`name=path`). `--plugins-dir`/`--hook-path` are appended only when the
/// host library resolved — the runtime then exposes it as the read-only
/// `horsie_shared` workspace and runs plugin SessionStart hooks.
pub fn runtime_args(
    endpoint: &str,
    runtime_id: &str,
    workspaces: &[String],
    plugins_dir: Option<&Path>,
    hook_path: &[PathBuf],
) -> Vec<String> {
    let mut args = vec![
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--runtime-id".to_string(),
        runtime_id.to_string(),
    ];
    for w in workspaces {
        args.push("--workspace".to_string());
        args.push(w.clone());
    }
    if let Some(dir) = plugins_dir {
        args.push("--plugins-dir".to_string());
        args.push(dir.display().to_string());
        for hp in hook_path {
            args.push("--hook-path".to_string());
            args.push(hp.display().to_string());
        }
    }
    args
}

/// One-line note about the host plugin library, printed when connecting with one.
pub fn plugins_summary(plugins_dir: &Path, count: usize) -> String {
    format!("plugins: {count} installed from {}", plugins_dir.display())
}

/// The resolved host plugin library handed to the spawned runtime: the library
/// root plus the hook interpreter dirs for its SessionStart hooks.
pub struct PluginLibrary {
    pub dir: PathBuf,
    pub hook_path: Vec<PathBuf>,
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
    plugins: Option<PluginLibrary>,
) -> Result<i32, CliError> {
    let endpoint = server_to_endpoint(server)?;
    let normalized: Vec<String> = workspaces
        .iter()
        .map(|w| normalize_workspace_arg(w))
        .collect();
    let args = runtime_args(
        &endpoint,
        runtime_id,
        &normalized,
        plugins.as_ref().map(|p| p.dir.as_path()),
        plugins.as_ref().map_or(&[], |p| p.hook_path.as_slice()),
    );

    let mut cmd = Command::new(runtime_bin);
    cmd.args(&args);

    println!("{}", connection_summary(server, runtime_id, &normalized));
    if let Some(p) = &plugins {
        println!(
            "{}",
            plugins_summary(&p.dir, crate::plugins::count_installed(&p.dir))
        );
    }
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
    fn runtime_args_omit_plugins_flags_without_library() {
        let args = runtime_args(
            "ws://h:3789/api/runtime/connect?register=local",
            "local",
            &["main=.".to_string()],
            None,
            &[],
        );
        assert_eq!(
            args,
            vec![
                "--endpoint",
                "ws://h:3789/api/runtime/connect?register=local",
                "--runtime-id",
                "local",
                "--workspace",
                "main=.",
            ]
        );
    }

    #[test]
    fn runtime_args_append_plugins_dir_and_hook_paths() {
        let args = runtime_args(
            "ws://h/x",
            "local",
            &["main=.".to_string()],
            Some(Path::new("/home/u/.local/share/horsie/plugins")),
            &[
                PathBuf::from("/opt/node/bin"),
                PathBuf::from("/usr/local/bin"),
            ],
        );
        let tail = &args[args.len() - 6..];
        assert_eq!(
            tail,
            [
                "--plugins-dir",
                "/home/u/.local/share/horsie/plugins",
                "--hook-path",
                "/opt/node/bin",
                "--hook-path",
                "/usr/local/bin",
            ]
        );
    }

    #[test]
    fn plugins_summary_renders_count_and_dir() {
        assert_eq!(
            plugins_summary(Path::new("/p"), 3),
            "plugins: 3 installed from /p"
        );
    }

    #[test]
    fn server_to_endpoint_maps_http_to_ws() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_maps_https_to_wss() {
        assert_eq!(
            server_to_endpoint("https://horsie.example.com").unwrap(),
            "wss://horsie.example.com/api/runtime/connect?register=local"
        );
    }

    #[test]
    fn server_to_endpoint_strips_trailing_slash() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789/").unwrap(),
            "ws://localhost:3789/api/runtime/connect?register=local"
        );
    }

    /// `register` is the vendor-kind discriminator, not the runtime's name:
    /// the server only fires the local-daemon hook for the literal `local`,
    /// and the runtime's identity travels via `RuntimeReady.runtime_id`.
    #[test]
    fn server_to_endpoint_always_registers_as_local() {
        assert!(
            server_to_endpoint("http://h:3789")
                .unwrap()
                .ends_with("?register=local")
        );
    }

    #[test]
    fn server_to_endpoint_rejects_non_http_scheme() {
        assert!(server_to_endpoint("ws://localhost:3789").is_err());
        assert!(server_to_endpoint("localhost:3789").is_err());
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
