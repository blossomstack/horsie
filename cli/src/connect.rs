//! `horsie connect`: run this machine as a **runtime vendor agent**.
//!
//! It holds one outbound WebSocket to the session server and spawns one
//! `horsie-runtime` child per session, each dialing this process's own unix
//! socket. That is the difference from the previous design, where `connect`
//! spawned a single runtime that every session shared: sessions now get
//! independent processes with a real lifecycle, so stopping or deleting one
//! cannot disturb another.
//!
//! Every runtime still works in the directories given by `--workspace`, so
//! concurrent sessions on one agent share those files.

use crate::error::CliError;
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, FixedWorkspaces, ProcessRuntimeProvider, RuntimeEndpoint,
    RuntimeListenerServer, RuntimeVendor, SandboxPolicy, serve_runtime_connections,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Translate a `--server` URL (`http(s)://host[:port]`) into the
/// `ws(s)://.../api/vendor/connect` endpoint the agent dials.
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
    Ok(format!("{ws_scheme}://{rest}/api/vendor/connect"))
}

/// A bare path (no `=`) becomes `main=<path>`; `name=path` passes through
/// unchanged.
pub fn normalize_workspace_arg(s: &str) -> String {
    if s.contains('=') {
        s.to_string()
    } else {
        format!("main={s}")
    }
}

/// Parse normalized `name=path` arguments into the agent's resolver table.
/// Relative paths are made absolute against the current directory, since the
/// spawned runtime does not necessarily inherit it.
pub fn parse_workspaces(workspaces: &[String]) -> Result<HashMap<String, PathBuf>, CliError> {
    let mut out = HashMap::new();
    for raw in workspaces {
        let normalized = normalize_workspace_arg(raw);
        let Some((name, path)) = normalized.split_once('=') else {
            return Err(CliError::Validation(format!(
                "--workspace must be 'name=path', got '{raw}'"
            )));
        };
        if name.is_empty() {
            return Err(CliError::Validation(format!(
                "--workspace name must not be empty in '{raw}'"
            )));
        }
        let path = PathBuf::from(path);
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|e| CliError::Io(format!("resolve current directory: {e}")))?
                .join(path)
        };
        if out.insert(name.to_string(), absolute).is_some() {
            return Err(CliError::Validation(format!(
                "--workspace '{name}' given more than once"
            )));
        }
    }
    Ok(out)
}

/// The one-line confirmation printed once the agent is connected.
pub fn connection_summary(server: &str, vendor_name: &str, workspaces: &[String]) -> String {
    let list = workspaces
        .iter()
        .map(|w| {
            let (name, path) = w.split_once('=').unwrap_or(("main", w.as_str()));
            format!("workspace \"{name}\" -> {path}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("connected to {server} as vendor \"{vendor_name}\" · {list}")
}

/// One-line note about the host plugin library, printed when serving one.
pub fn plugins_summary(plugins_dir: &Path, count: usize) -> String {
    format!("plugins: {count} installed from {}", plugins_dir.display())
}

/// The resolved host plugin library served to every runtime this agent spawns.
pub struct PluginLibrary {
    pub dir: PathBuf,
    pub hook_path: Vec<PathBuf>,
}

/// Warn when more than one workspace maps to the same directory, and when the
/// agent is about to serve concurrent sessions out of a shared tree.
fn shared_directory_notice(workspaces: &HashMap<String, PathBuf>) -> String {
    let dirs: Vec<String> = {
        let mut d: Vec<String> = workspaces
            .values()
            .map(|p| p.display().to_string())
            .collect();
        d.sort();
        d.dedup();
        d
    };
    format!(
        "note: every session on this vendor works in {}; \
         concurrent sessions will edit the same files",
        dirs.join(", ")
    )
}

/// Run this machine as a vendor agent until the socket closes or the process is
/// interrupted.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    runtime_bin: &Path,
    server: &str,
    workspaces: &[String],
    vendor_name: &str,
    background: bool,
    state_dir: &Path,
    plugins: Option<PluginLibrary>,
    sandbox: bool,
) -> Result<i32, CliError> {
    let endpoint = server_to_endpoint(server)?;
    let normalized: Vec<String> = workspaces
        .iter()
        .map(|w| normalize_workspace_arg(w))
        .collect();
    let table = parse_workspaces(workspaces)?;

    if background {
        return Err(CliError::Validation(
            "--background is no longer supported: `horsie connect` is now a long-lived \
             vendor agent that supervises one runtime per session. Run it under your \
             process manager (systemd, launchd, tmux) so its lifetime and logs are managed \
             explicitly."
                .to_string(),
        ));
    }

    std::fs::create_dir_all(state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    let socket = state_dir.join("vendor-runtimes.sock");
    // A stale socket from a previous run would make bind fail.
    let _ = std::fs::remove_file(&socket);

    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(socket.clone()))
        .await
        .map_err(|e| CliError::Executor(format!("bind runtime socket: {e}")))?;
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());

    let bin = runtime_bin.to_path_buf();
    let sock_for_provider = socket.clone();
    let registry_for_provider = connected.clone();
    let provider: horsie_runtime_vendor::ProviderFactory =
        Arc::new(move |_runtime_id: &str, caps: Option<PathBuf>| {
            let mut p = ProcessRuntimeProvider::new(
                bin.clone(),
                RuntimeEndpoint::Unix(sock_for_provider.clone()),
                registry_for_provider.clone(),
            );
            if let Some(capabilities_file) = caps {
                p = p.with_sandbox(SandboxPolicy { capabilities_file });
            }
            Arc::new(p)
        });

    let mut agent = RuntimeVendor::new(
        vendor_name.to_string(),
        // A fixed, user-owned directory: no repo checkout, no bundle install.
        false,
        provider,
        connected,
        Arc::new(FixedWorkspaces::new(table.clone())),
        state_dir.join("runtimes"),
    )
    .with_sandbox(sandbox)
    .with_bundles(horsie_runtime_vendor::BundleDelivery {
        // The runtimes run on this machine, so whatever address reaches the
        // server from here reaches it from them.
        base_url: server.trim_end_matches('/').to_string(),
        dir: state_dir.join("bundles").to_string_lossy().into_owned(),
        cache_dir: Some(
            state_dir
                .join("bundle-cache")
                .to_string_lossy()
                .into_owned(),
        ),
    });
    if let Some(p) = &plugins {
        agent = agent.with_host_library(p.dir.clone(), p.hook_path.clone());
    }

    println!("{}", connection_summary(server, vendor_name, &normalized));
    if let Some(p) = &plugins {
        println!(
            "{}",
            plugins_summary(&p.dir, crate::plugins::count_installed(&p.dir))
        );
    }
    println!("{}", shared_directory_notice(&table));
    println!("open {server} in your browser to start a session");

    let signal = install_signal_handler().map_err(CliError::Io)?;
    let run_cancel = cancel.clone();
    tokio::select! {
        result = agent.run(&endpoint, cancel.clone()) => {
            result.map_err(CliError::Executor)?;
            Ok(0)
        }
        () = signal => {
            run_cancel.cancel();
            Ok(0)
        }
    }
}

#[cfg(unix)]
fn install_signal_handler() -> Result<impl std::future::Future<Output = ()> + Send, String> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT: {e}"))?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn install_signal_handler() -> Result<impl std::future::Future<Output = ()> + Send, String> {
    let ctrl_c = tokio::signal::ctrl_c().map_err(|e| format!("ctrl-c: {e}"))?;
    Ok(async move {
        let _ = ctrl_c.await;
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_url_becomes_the_vendor_connect_endpoint() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789").unwrap(),
            "ws://localhost:3789/api/vendor/connect"
        );
        assert_eq!(
            server_to_endpoint("https://horsie.example.com").unwrap(),
            "wss://horsie.example.com/api/vendor/connect"
        );
        assert_eq!(
            server_to_endpoint("http://localhost:3789/").unwrap(),
            "ws://localhost:3789/api/vendor/connect"
        );
    }

    #[test]
    fn server_url_must_be_http_or_https() {
        assert!(server_to_endpoint("localhost:3789").is_err());
        assert!(server_to_endpoint("ws://localhost:3789").is_err());
    }

    #[test]
    fn bare_workspace_path_defaults_to_main() {
        assert_eq!(normalize_workspace_arg("/tmp/x"), "main=/tmp/x");
        assert_eq!(normalize_workspace_arg("docs=/tmp/y"), "docs=/tmp/y");
    }

    #[test]
    fn workspaces_parse_into_absolute_paths() {
        let table = parse_workspaces(&["/tmp/project".to_string()]).unwrap();
        assert_eq!(table.get("main").unwrap(), &PathBuf::from("/tmp/project"));

        let named =
            parse_workspaces(&["docs=/tmp/d".to_string(), "src=/tmp/s".to_string()]).unwrap();
        assert_eq!(named.len(), 2);
        assert_eq!(named.get("docs").unwrap(), &PathBuf::from("/tmp/d"));
    }

    #[test]
    fn a_relative_workspace_path_is_resolved_against_the_cwd() {
        let table = parse_workspaces(&["sub/dir".to_string()]).unwrap();
        let resolved = table.get("main").unwrap();
        assert!(
            resolved.is_absolute(),
            "the spawned runtime does not inherit our cwd, so the path must be absolute: {}",
            resolved.display()
        );
        assert!(resolved.ends_with("sub/dir"));
    }

    #[test]
    fn a_duplicate_workspace_name_is_rejected() {
        let err = parse_workspaces(&["main=/tmp/a".to_string(), "main=/tmp/b".to_string()])
            .expect_err("duplicate names are ambiguous");
        assert!(format!("{err}").contains("more than once"), "{err}");
    }

    #[test]
    fn connection_summary_names_the_vendor_and_every_workspace() {
        let summary = connection_summary(
            "http://localhost:3789",
            "my-laptop",
            &["main=/tmp/a".to_string(), "docs=/tmp/b".to_string()],
        );
        assert!(summary.contains("vendor \"my-laptop\""), "{summary}");
        assert!(
            summary.contains("workspace \"main\" -> /tmp/a"),
            "{summary}"
        );
        assert!(
            summary.contains("workspace \"docs\" -> /tmp/b"),
            "{summary}"
        );
    }

    #[test]
    fn the_shared_directory_notice_names_each_directory_once() {
        let mut table = HashMap::new();
        table.insert("main".to_string(), PathBuf::from("/tmp/project"));
        table.insert("also".to_string(), PathBuf::from("/tmp/project"));
        let notice = shared_directory_notice(&table);
        assert_eq!(
            notice.matches("/tmp/project").count(),
            1,
            "one directory, mentioned once: {notice}"
        );
        assert!(notice.contains("same files"), "{notice}");
    }
}
