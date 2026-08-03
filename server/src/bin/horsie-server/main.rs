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
use config::{BootConfig, BootError, JournalBackend};
use horsie_actor::{FileJournal, Journal, spawn_root};
use horsie_models::settings::ServerInfo;
use horsie_server::config::{DbConfigStore, StoreDeps, model_cards};
use horsie_server::http::{AppState, app};
use horsie_server::journal::SqliteJournal;
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
    /// JSON file of extra model cards to seed at startup (insert-if-missing;
    /// bundled defaults are always seeded). Also read from
    /// $HORSIE_MODEL_CARDS_SEED.
    #[arg(long)]
    model_cards_seed: Option<PathBuf>,
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

    let db_url = resolve_db_url(&cfg, &data_dir);
    let info = ServerInfo {
        config_path: config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        database: redact_db_url(&db_url),
        state_dir: cfg.storage.state_dir.display().to_string(),
        data_dir: cfg.storage.data_dir.display().to_string(),
        plugins_dir: data_dir.join("plugins").display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let opened = DbConfigStore::open(&db_url, StoreDeps { info })
        .await
        .map_err(BootError::Config)?;

    // Built after the store, because the SQLite backend shares its pool — one
    // database, one migrator, one set of connections.
    let journal: Arc<dyn Journal> = match cfg.storage.journal {
        JournalBackend::Sqlite => Arc::new(SqliteJournal::new(opened.pool.clone())),
        JournalBackend::File => Arc::new(FileJournal::new(data_dir.clone())),
    };

    // Seed the model-card catalog: bundled defaults plus an optional operator
    // file. Seed-file parse/read errors are fatal (operator input should fail
    // loud); DB errors only warn — the admin API stays usable to fix state.
    // Insert-if-missing semantics mean admin edits survive every restart.
    let model_cards = std::sync::Arc::new(model_cards::ModelCardStore::new(opened.db.clone()));
    let seed_path = cli
        .model_cards_seed
        .clone()
        .or_else(|| std::env::var_os("HORSIE_MODEL_CARDS_SEED").map(PathBuf::from));
    let seeding = (|| -> Result<Vec<horsie_models::model_cards::ModelCardInput>, BootError> {
        let mut seeds = model_cards::bundled_seed().map_err(BootError::Config)?;
        if let Some(path) = seed_path {
            seeds.extend(model_cards::load_seed_file(&path).map_err(BootError::Config)?);
        }
        Ok(seeds)
    })();
    match seeding {
        Ok(seeds) => {
            if let Err(e) = model_cards.seed_if_missing(&seeds).await {
                eprintln!("warning: seeding model cards failed: {e:?}");
            }
        }
        Err(e) => return Err(e),
    }

    // Vendor agents publish themselves into the same map sessions select from,
    // exactly as the local-daemon registry does for dial-in runtimes.
    let vendor_agents = Arc::new(horsie_server::runtime_vendor::RuntimeVendorRegistry::new(
        opened.vendors.clone(),
    ));

    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.db.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.db.clone()),
        github.clone(),
    ));
    let plugins = Arc::new(PluginService::new(
        PluginStore::new(opened.db.clone()),
        ArtifactStore::new(data_dir.join("plugins")),
        artifact_secret(),
    ));
    let memory = Arc::new(horsie_server::memory::MemoryService::new(
        horsie_server::memory::MemoryStore::new(opened.db.clone()),
    ));
    let agents = Arc::new(horsie_server::agents::AgentService::new(
        horsie_server::agents::AgentStore::new(opened.db.clone()),
        opened.store.clone(),
    ));
    let routines = Arc::new(horsie_server::routines::RoutineService::new(
        horsie_server::routines::RoutineStore::new(opened.pool.clone()),
        agents.clone(),
    ));

    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(opened.db.clone()),
        horsie_server::auth::AuthDeps {
            enabled: config::auth_enabled(&cfg),
            state_dir: state_dir.clone(),
        },
    ));
    match auth.bootstrap().await {
        Ok(Some(password)) => {
            let file = state_dir
                .join(horsie_server::auth::INITIAL_PASSWORD_FILE)
                .display()
                .to_string();
            println!(
                "\n\
                 ┌──────────────────────────────────────────────────────────────┐\n\
                 │  horsie created its admin account                            │\n\
                 └──────────────────────────────────────────────────────────────┘\n\
                 \n  username: admin\n  password: {password}\n\n\
                 Also written to {file} (deleted when you change the password).\n\
                 Change it from Settings → Account.\n"
            );
        }
        Ok(None) => {}
        Err(e) => {
            return Err(BootError::Config(format!(
                "bootstrapping the admin account: {e}"
            )));
        }
    }
    if !auth.enabled() {
        println!(
            "warning: authentication is disabled — every caller that can reach \
             this port has full access"
        );
    }

    let runtimes = Arc::new(horsie_server::runtime_manager::RuntimeManager::new(
        horsie_server::runtime_manager::RuntimeDeps {
            vendors: opened.vendors.clone(),
            state_dir: state_dir.clone(),
            github_tokens: Some(github.clone()),
            plugins: Some(plugins.clone() as Arc<dyn horsie_server::plugins::PluginProvisioner>),
        },
    ));
    let deps = ServerDeps {
        runtimes,
        provider_registry: opened.registry,
        vendors: opened.vendors,
        state_dir,
        github_tokens: Some(github.clone()),
        mcp: Some(mcp.clone()),
        plugins: Some(plugins.clone() as Arc<dyn horsie_server::plugins::PluginProvisioner>),
        memory: Some(memory.clone()),
    };
    let (global_tx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(
        SessionSupervisor::new(deps, global_tx.clone()),
        journal.clone(),
    );

    // Triggering a routine is one code path; the timer is a clock on top of it.
    let routine_runner = Arc::new(horsie_server::routines::RoutineRunner::new(
        routines.clone(),
        agents.clone(),
        opened.store.clone(),
        vendor_agents.clone(),
        supervisor.clone(),
    ));
    Arc::new(horsie_server::routines::RoutineScheduler::new(
        routine_runner.clone(),
        routines.clone(),
    ))
    .spawn();

    let state = AppState {
        supervisor,
        global_events: global_tx,
        auth,
        config_store: opened.store,
        model_cards,
        github,
        mcp,
        plugins,
        memory,
        agents,
        routines,
        routine_runner,
        vendor_agents,
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
