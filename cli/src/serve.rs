//! `horsie serve`: the standalone session server (HTTP + SSE).
//!
//! Deployment/bootstrap config comes from `config.json`/env (storage, sandbox,
//! runtime, hackamore, and the settings-DB location). The runtime-editable
//! settings — providers, models, vendors, default vendor — live in the settings
//! database (owned by the `server` crate) and are managed from the web UI, never
//! overlapping with the file. Journal root is `<data_dir>/server`, state root
//! `<state_dir>/server`, so the server runs alongside the job daemon untouched.

use crate::capabilities;
use crate::config::HorsieConfig;
use crate::error::CliError;
use horsie_actor::{FileJournal, Journal, spawn_root};
use horsie_models::capabilities::CapabilitySpec;
use horsie_models::settings::ServerInfo;
use horsie_server::config::{DbConfigStore, StoreDeps};
use horsie_server::http::{AppState, app};
use horsie_server::plugins::{ArtifactStore, PluginService, PluginStore};
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::SessionSupervisor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub async fn serve(
    cfg: HorsieConfig,
    config_path: Option<PathBuf>,
    addr: String,
    web_dir: Option<PathBuf>,
) -> Result<(), CliError> {
    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| CliError::Io(e.to_string()))?;

    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));

    // Resolve the default capability spec once, and a finalizer that applies path
    // expansion + plugin grants + platform seatbelt rules to request-supplied specs.
    let default_caps = match &cfg.sandbox.capabilities_file {
        Some(path) => CapabilitySpec::load(path).map_err(CliError::Config)?,
        None => capabilities::builtin_default()?,
    };
    let plugins_dir = crate::plugins::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        crate::plugins::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else {
        Vec::new()
    };
    let (pd, hp) = (plugins_dir.clone(), hook_path.clone());
    let caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync> =
        Arc::new(move |caps| {
            let spec = capabilities::with_plugin_grants(
                capabilities::resolve_user_paths(caps),
                pd.as_deref(),
                &hp,
            );
            capabilities::with_default_seatbelt_rules(spec)
        });
    let default_caps = caps_finalize(default_caps);

    // Open the settings database (server-owned) and get the live registry +
    // vendors it builds. Providers/models/vendors are the DB's concern, not the
    // file's — warn if the file still carries them.
    if !cfg.providers.is_empty() || !cfg.models.is_empty() {
        eprintln!(
            "note: `horsie serve` ignores config.json providers/models — manage them in Settings"
        );
    }
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
    .map_err(CliError::Config)?;

    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.pool.clone()),
        github.clone(),
    ));

    // Plugin-bundle library: artifacts live on the data volume beside the
    // journal; the token secret comes from the env or is random per-process
    // (tokens are short-lived and minted fresh at each provisioning).
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
        web_dir,
    };
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| CliError::Executor(format!("bind {addr}: {e}")))?;
    println!("horsie server listening on http://{addr}");
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    axum::serve(listener, app(state))
        .await
        .map_err(|e| CliError::Executor(e.to_string()))
}

/// The settings-database URL: `$HORSIE_DATABASE_URL`, else `database.url` from
/// config, else a SQLite file under the server data dir.
fn resolve_db_url(cfg: &HorsieConfig, data_dir: &Path) -> String {
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
/// if set, else 32 random bytes (fine per-process — tokens are short-lived and
/// minted on demand at provisioning, never persisted across a restart).
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
