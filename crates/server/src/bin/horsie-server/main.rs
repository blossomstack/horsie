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
use horsie_server::auth::AuthMode;
use horsie_server::boot::{BootOptions, Booted};
use horsie_server::config::model_cards;
use horsie_server::http::{AppState, app};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// What is logged when `RUST_LOG` says nothing usable. `info` because the
/// events this exists for — a refused boot, a failed recovery — are `error`
/// and `warn`, and the level above them costs a handful of lines a start.
const DEFAULT_LOG_FILTER: &str = "info";

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
    // Before anything opens an https:// or wss:// connection.
    horsie_support::tls::install_crypto_provider();
    init_tracing();
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Install the subscriber that decides where this process's `tracing` events
/// go.
///
/// Without one they go nowhere. The server narrates the things an operator
/// needs when something is wrong — an actor that could not replay its journal
/// says so through `tracing::error!` and then closes its mailbox — and for as
/// long as no subscriber was installed the only visible symptom was every
/// session route answering `500 session supervisor unavailable`, with two
/// lines in the container log and no cause anywhere.
///
/// First thing in `main`, ahead of argument parsing, so a failure during boot
/// is narrated too.
fn init_tracing() {
    // An *empty* RUST_LOG is treated as unset, not as "log nothing". The
    // difference matters because it is what a deployment produces by accident:
    // `RUST_LOG: ${RUST_LOG:-info}` in a compose file substitutes the empty
    // string when the variable is merely undefined, and an empty filter has no
    // directives, so it passes no event at any level — which is the silence
    // this function exists to end, arrived at by a different route.
    let requested = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty());
    // A malformed directive downgrades to the default rather than aborting the
    // process: losing the filter someone asked for is a smaller loss than a
    // server that will not start, and the complaint says which it did.
    let filter = match requested.as_deref().map(EnvFilter::try_new) {
        Some(Ok(filter)) => filter,
        Some(Err(e)) => {
            eprintln!("ignoring RUST_LOG: {e}");
            EnvFilter::new(DEFAULT_LOG_FILTER)
        }
        None => EnvFilter::new(DEFAULT_LOG_FILTER),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run(cli: Cli) -> Result<(), BootError> {
    let cfg = BootConfig::resolve(cli.config.as_deref())?;
    let config_path = BootConfig::resolve_path(cli.config.as_deref());
    let db_url_env = std::env::var("HORSIE_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let booted = boot(&cfg, &cli, config_path, db_url_env).await?;
    announce(&booted, &cfg);
    let state = booted.state;

    let listener = tokio::net::TcpListener::bind(&cli.addr)
        .await
        .map_err(|e| BootError::Io(format!("bind {}: {e}", cli.addr)))?;
    println!("horsie server listening on http://{}", cli.addr);
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    serve(listener, state).await
}

/// Map this binary's file/CLI config onto the library's boot options.
///
/// Split from [`run`] so a test can bring up the real composition root: the
/// only two inputs `run` reads from the process itself — the config file and
/// `$HORSIE_DATABASE_URL` — are parameters here, so a test cannot accidentally
/// boot against a developer's own deployment.
async fn boot(
    cfg: &BootConfig,
    cli: &Cli,
    config_path: Option<PathBuf>,
    db_url_env: Option<String>,
) -> Result<Booted, BootError> {
    // Operator input should fail loud, so a bad seed file is fatal here rather
    // than a card quietly missing from a catalogue later.
    let seed_path = cli
        .model_cards_seed
        .clone()
        .or_else(|| std::env::var_os("HORSIE_MODEL_CARDS_SEED").map(PathBuf::from));
    let extra_model_cards = match seed_path {
        Some(path) => model_cards::load_seed_file(&path).map_err(BootError::Config)?,
        None => Vec::new(),
    };

    horsie_server::boot::boot(BootOptions {
        state_dir: cfg.storage.state_dir.clone(),
        data_dir: cfg.storage.data_dir.clone(),
        db_url: db_url_env.or_else(|| cfg.database.url.clone()),
        max_connections: cfg
            .database
            .max_connections
            .unwrap_or(horsie_server::config::DEFAULT_MAX_CONNECTIONS),
        auth_mode: config::auth_mode(cfg),
        extra_model_cards,
        web_dir: cli.web.clone(),
        config_path,
        // Same precedence as the database URL: the environment wins, so a
        // deployment can point a node at its bus without editing a file.
        bus_url: std::env::var("HORSIE_BUS_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| cfg.bus.url.clone()),
        cluster: cfg.cluster.clone(),
    })
    .await
    .map_err(BootError::Config)
}

/// Everything a person at a terminal needs to be told about this boot.
fn announce(booted: &Booted, cfg: &BootConfig) {
    if let Some(password) = booted.initial_password.as_deref() {
        let file = booted
            .state_dir
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
    match config::auth_mode(cfg) {
        AuthMode::Off => println!(
            "warning: authentication is disabled — every caller that can reach \
             this port has full access"
        ),
        AuthMode::Delegated => println!(
            "authentication is delegated: this server serves no credential \
             routes and expects the layer in front of it to identify every caller"
        ),
        AuthMode::Password => {}
    }
}

/// Serve until the process ends. Split from [`run`] so a test can hold the
/// bound listener — and its address — and still exercise the real state that
/// `run` assembled.
async fn serve(listener: tokio::net::TcpListener, state: AppState) -> Result<(), BootError> {
    axum::serve(listener, app(state))
        .await
        .map_err(|e| BootError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    /// A boot against a throwaway directory, with no config file and no
    /// `$HORSIE_DATABASE_URL` — so it cannot reach a developer's own database.
    async fn boot_in(dir: &tempfile::TempDir) -> AppState {
        let cfg = BootConfig {
            storage: config::StorageConfig {
                state_dir: dir.path().join("state"),
                data_dir: dir.path().join("data"),
            },
            database: config::DatabaseConfig::default(),
            bus: config::BusConfig::default(),
            auth: config::AuthConfig {
                mode: config::AuthModeSetting::Off,
            },
            cluster: None,
        };
        let cli = Cli {
            config: None,
            addr: "127.0.0.1:0".into(),
            web: None,
            model_cards_seed: None,
        };
        boot(&cfg, &cli, None, None)
            .await
            .expect("the server boots")
            .state
    }

    async fn read_json<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    /// The gap that let two boot bugs through #217 with the Rust suite green:
    /// nothing in the tree ran this function. It asserts the parts that were
    /// broken then — the server comes up, and the account it bootstrapped can
    /// see the rows the migrations left for it.
    ///
    /// Since `0040_projects.sql` the chain has one more link and this covers it
    /// too: the rows belong to a *project*, and the account's default project
    /// has to be the one the migration seeded rather than a freshly minted one.
    /// A fresh one would leave a healthy, empty deployment holding all its data
    /// — the exact shape of the #217 near-miss, one level down.
    #[tokio::test]
    async fn a_fresh_deployment_boots_and_its_account_owns_its_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = boot_in(&dir).await;
        let project = state
            .shared
            .project_service
            .default_project(&state.shared.anonymous)
            .await
            .expect("the bootstrapped account resolves a default project")
            .id;
        let url = |path: &str| format!("/api/p/{project}{path}");
        let app = app(state.clone());

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("health responds");
        assert_eq!(res.status(), StatusCode::OK);

        // The near-miss in #217: `bootstrap` minted an id the migration's
        // backfill had not used, so the account came up healthy and empty. The
        // seeded memory space is the cheapest thing that proves otherwise.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(url("/memory-spaces"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("memory spaces respond");
        assert_eq!(res.status(), StatusCode::OK);
        let spaces: Vec<horsie_models::memory::MemorySpaceView> = read_json(res).await;
        assert_eq!(
            spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["default"],
            "the bootstrapped account's default project must own what the \
             migrations backfilled"
        );

        // Seeding moved off the boot path and onto the account's first touch,
        // so this is also the assertion that the lazy seed actually runs.
        let res = app
            .oneshot(
                Request::builder()
                    .uri(url("/model-cards"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("model cards respond");
        let cards: Vec<horsie_models::model_cards::ModelCard> = read_json(res).await;
        assert!(
            !cards.is_empty(),
            "the bundled catalogue is seeded on an account's first request"
        );
    }

    /// Booting twice over the same directory is what every restart does.
    #[tokio::test]
    async fn a_second_boot_over_the_same_data_reuses_the_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = boot_in(&dir).await;
        let second = boot_in(&dir).await;
        assert_eq!(
            first.shared.anonymous, second.shared.anonymous,
            "a restart must resolve the same account, or it comes up empty"
        );
    }

    /// The precedence `run` applies before handing a value to the library:
    /// environment, then the file, then the library's own default.
    #[test]
    fn the_database_url_prefers_the_environment_then_the_file() {
        let from_file: BootConfig =
            serde_json::from_str(r#"{ "database": { "url": "sqlite://file.db" } }"#).unwrap();
        let pick = |env: Option<String>, cfg: &BootConfig| {
            env.filter(|s: &String| !s.is_empty())
                .or_else(|| cfg.database.url.clone())
        };

        assert_eq!(
            pick(Some("postgres://env/db".into()), &from_file).as_deref(),
            Some("postgres://env/db")
        );
        assert_eq!(pick(None, &from_file).as_deref(), Some("sqlite://file.db"));
        // An empty environment value is absent, not an override.
        assert_eq!(
            pick(Some(String::new()), &from_file).as_deref(),
            Some("sqlite://file.db")
        );
        // Nothing anywhere: the library falls back to a file under data_dir.
        assert_eq!(pick(None, &BootConfig::default()), None);
    }
}
