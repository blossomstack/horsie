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
use horsie_models::settings::ServerInfo;
use horsie_server::config::model_cards;
use horsie_server::http::{AppState, app};
use horsie_server::plugins::ArtifactStore;
use horsie_server::routines::RoutineScheduler;
use horsie_server::users::{Shared, UserRegistry};
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
    let db_url_env = std::env::var("HORSIE_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let state = boot(&cfg, &cli, config_path, db_url_env).await?;

    let listener = tokio::net::TcpListener::bind(&cli.addr)
        .await
        .map_err(|e| BootError::Io(format!("bind {}: {e}", cli.addr)))?;
    println!("horsie server listening on http://{}", cli.addr);
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    serve(listener, state).await
}

/// Assemble everything the server serves from, and nothing about serving it.
///
/// Split out of [`run`] so a test can bring up the real composition root: the
/// only two inputs `run` reads from the process itself — the config file and
/// `$HORSIE_DATABASE_URL` — are parameters here, so a test cannot accidentally
/// boot against a developer's own deployment.
async fn boot(
    cfg: &BootConfig,
    cli: &Cli,
    config_path: Option<PathBuf>,
    db_url_env: Option<String>,
) -> Result<AppState, BootError> {
    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| BootError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| BootError::Io(e.to_string()))?;

    let db_url = resolve_db_url(db_url_env, cfg, &data_dir);
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
    // The pool comes up on its own first. Every store below binds a user, and
    // the user comes from the account `bootstrap` creates — which needs the
    // database. So the order is: open, bootstrap, then build everything scoped.
    let db = horsie_server::db::Db::open(
        &db_url,
        cfg.database
            .max_connections
            .unwrap_or(horsie_server::config::DEFAULT_MAX_CONNECTIONS),
    )
    .await
    .map_err(BootError::Config)?;

    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(db.clone()),
        horsie_server::auth::AuthDeps {
            enabled: config::auth_enabled(cfg),
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

    // The account `Principal::Anonymous` resolves to — every request on a
    // deployment with authentication disabled. Resolved here, once, because
    // `bootstrap` above is what guarantees there is one.
    let anonymous = auth
        .sole_user()
        .await
        .map_err(BootError::Config)?
        .ok_or_else(|| BootError::Config("no account exists after bootstrap".into()))?;

    // Resolve the model-card catalogue once. Parse/read errors are fatal here
    // (operator input should fail loud); each account is then seeded from it on
    // first touch rather than every account at every boot.
    let seed_path = cli
        .model_cards_seed
        .clone()
        .or_else(|| std::env::var_os("HORSIE_MODEL_CARDS_SEED").map(PathBuf::from));
    let mut model_card_seed = model_cards::bundled_seed().map_err(BootError::Config)?;
    if let Some(path) = seed_path {
        model_card_seed.extend(model_cards::load_seed_file(&path).map_err(BootError::Config)?);
    }

    let shared = Arc::new(Shared {
        db,
        artifacts: Arc::new(ArtifactStore::new(data_dir.join("plugins"))),
        artifact_secret: Arc::new(artifact_secret()),
        info,
        model_card_seed_marker: model_cards::seed_marker(&model_card_seed),
        model_card_seed: Arc::new(model_card_seed),
        anonymous,
        supervisor: horsie_server::sessions::supervisor::SupervisorConfig::default(),
    });
    let users = Arc::new(UserRegistry::new(shared.clone()));

    // One timer for the deployment, over every account's routines. It resolves
    // an owner's services when one of their routines comes due, which is also
    // the only thing that builds a bundle nobody has made a request for.
    Arc::new(RoutineScheduler::new(shared.db.clone(), users.clone())).spawn();

    Ok(AppState {
        auth,
        shared,
        users,
        web_dir: cli.web.clone(),
    })
}

/// Serve until the process ends. Split from [`run`] so a test can hold the
/// bound listener — and its address — and still exercise the real state that
/// `run` assembled.
async fn serve(listener: tokio::net::TcpListener, state: AppState) -> Result<(), BootError> {
    axum::serve(listener, app(state))
        .await
        .map_err(|e| BootError::Io(e.to_string()))
}

/// `$HORSIE_DATABASE_URL` (passed in), else `database.url` from config, else a
/// SQLite file under the server data dir.
fn resolve_db_url(env: Option<String>, cfg: &BootConfig, data_dir: &Path) -> String {
    if let Some(v) = env.filter(|s| !s.is_empty()) {
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
            auth: config::AuthConfig { enabled: false },
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
    #[tokio::test]
    async fn a_fresh_deployment_boots_and_its_account_owns_its_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = boot_in(&dir).await;
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
                    .uri("/api/memory-spaces")
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
            "the bootstrapped account must own what the migrations backfilled"
        );

        // Seeding moved off the boot path and onto the account's first touch,
        // so this is also the assertion that the lazy seed actually runs.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/model-cards")
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

    #[test]
    fn the_database_url_prefers_the_environment_then_the_file() {
        let cfg = BootConfig::default();
        let dir = Path::new("/tmp/horsie-test");
        assert_eq!(
            resolve_db_url(Some("postgres://env/db".into()), &cfg, dir),
            "postgres://env/db"
        );
        assert_eq!(
            resolve_db_url(None, &cfg, dir),
            "sqlite:///tmp/horsie-test/config.db"
        );
        // An empty environment value is absent, not an override.
        assert_eq!(
            resolve_db_url(Some(String::new()), &cfg, dir),
            "sqlite:///tmp/horsie-test/config.db"
        );
    }
}
