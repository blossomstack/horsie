#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

//! `horsie-server`: the standalone session server (HTTP + SSE).
//!
//! Deployment/bootstrap config comes from `config.json`/env (storage, the
//! shared local-runtime-vendor listener, and the settings-DB location). The
//! runtime-editable settings — providers, models, vendors, default vendor —
//! live in the settings database (owned by the `horsie-server` library) and
//! are managed from the web UI, never overlapping with the file.

mod config;

use clap::Parser;
use config::{BootConfig, BootError};
use horsie_actor::{FileJournal, Journal, spawn_root};
use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
use horsie_models::settings::ServerInfo;
use horsie_server::config::{DbConfigStore, StoreDeps};
use horsie_server::http::{AppState, CapsFinalize, app};
use horsie_server::plugins::{ArtifactStore, PluginService, PluginStore};
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::SessionSupervisor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "horsie-server",
    version,
    about = "Session-oriented HTTP + SSE server for horsie"
)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Bind address for the HTTP server. Use `0.0.0.0:3789` to accept
    /// connections from other hosts on the network.
    #[arg(long, default_value = "127.0.0.1:3789")]
    addr: String,
    /// Directory of built web-UI assets to serve alongside the API (e.g.
    /// `clients/web/dist`). When set, the UI is served same-origin, so no
    /// separate dev server or CORS setup is needed.
    #[arg(long)]
    web: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), BootError> {
    let cfg = BootConfig::resolve(cli.config.as_deref())?;
    let config_path = BootConfig::resolve_path(cli.config.as_deref());

    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| BootError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| BootError::Io(e.to_string()))?;

    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));

    // No vendor enforces the per-session capability spec today (the old
    // server-spawned sandboxed local vendor was replaced by the user-launched
    // LocalDaemonVendor in #8) — supply a fixed minimal default and pass any
    // request-supplied spec through unchanged. Matches `AppState`'s own doc
    // comment ("injected by the host binary, which owns the capability-
    // resolution helpers") and the fallback the crate's own tests already use.
    let caps_finalize: CapsFinalize = Arc::new(|caps| caps);
    let default_caps = CapabilitySpec {
        network: NetworkPolicy::Block(BlockNetwork {}),
        grants: vec![],
        unsafe_seatbelt_rules: None,
    };

    let plugins_dir = config::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        config::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else {
        Vec::new()
    };

    let db_url = resolve_db_url(&cfg, &data_dir);
    let info = ServerInfo {
        config_path: config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        database: redact_db_url(&db_url),
        state_dir: cfg.storage.state_dir.display().to_string(),
        data_dir: cfg.storage.data_dir.display().to_string(),
        plugins_dir: cfg.storage.plugins_dir.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let opened = DbConfigStore::open(
        &db_url,
        StoreDeps {
            info,
            local_runtime_listen: cfg.local_runtime_listen.clone(),
        },
    )
    .await
    .map_err(BootError::Config)?;

    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.pool.clone()),
        github.clone(),
    ));
    let plugins = Arc::new(PluginService::new(
        PluginStore::new(opened.pool.clone()),
        ArtifactStore::new(data_dir.join("plugins")),
        artifact_secret(),
    ));

    let deps = ServerDeps {
        provider_registry: opened.registry,
        vendors: opened.vendors,
        state_dir,
        github_tokens: Some(github.clone()),
        mcp: Some(mcp.clone()),
        plugins: Some(plugins.clone() as Arc<dyn horsie_server::plugins::PluginProvisioner>),
    };
    let (global_tx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(
        SessionSupervisor::new(deps, global_tx.clone()),
        journal.clone(),
    );

    let state = AppState {
        supervisor,
        journal,
        global_events: global_tx,
        caps_finalize,
        default_caps,
        plugins_dir,
        hook_path,
        config_store: opened.store,
        github,
        mcp,
        plugins,
        web_dir: cli.web,
    };
    let listener = tokio::net::TcpListener::bind(&cli.addr)
        .await
        .map_err(|e| BootError::Io(format!("bind {}: {e}", cli.addr)))?;
    println!("horsie server listening on http://{}", cli.addr);
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    axum::serve(listener, app(state))
        .await
        .map_err(|e| BootError::Io(e.to_string()))
}

/// `$HORSIE_DATABASE_URL`, else `database.url` from config, else a SQLite
/// file under the server data dir.
fn resolve_db_url(cfg: &BootConfig, data_dir: &Path) -> String {
    if let Ok(v) = std::env::var("HORSIE_DATABASE_URL")
        && !v.is_empty()
    {
        return v;
    }
    if let Some(u) = cfg.database.url.as_ref().filter(|s| !s.is_empty()) {
        return u.clone();
    }
    format!("sqlite://{}/config.db", data_dir.display())
}

/// The HS256 secret for artifact capability tokens: `$HORSIE_ARTIFACT_SECRET`
/// if set, else 32 random bytes (fine per-process — tokens are short-lived).
fn artifact_secret() -> Vec<u8> {
    std::env::var("HORSIE_ARTIFACT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .map(String::into_bytes)
        .unwrap_or_else(|| {
            let mut v = uuid::Uuid::new_v4().as_bytes().to_vec();
            v.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
            v
        })
}

/// Hide credentials in a database URL's authority (e.g. `postgres://u:p@host`).
fn redact_db_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((auth, tail)) = rest.split_once('@')
        && auth.contains(':')
    {
        return format!("{scheme}://***@{tail}");
    }
    url.to_string()
}
