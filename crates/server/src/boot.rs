//! Assembling everything the server serves from, and nothing about serving it.
//!
//! This lives in the library rather than in `bin/horsie-server` because it is
//! the only way to stand a server up: the pool, the account bootstrap, the
//! artifact store, the model-card catalogue and the routine timer have to
//! happen in one order, and that order is not obvious from the outside. A
//! second binary — one that wraps horsie behind its own middleware, say —
//! would otherwise copy it and drift from it at the first change.
//!
//! What stays with the caller is everything about *policy*: where the config
//! file lives, how to parse it, what to print, and whether to serve.

use crate::auth::{AuthDeps, AuthMode, AuthService, AuthStore};
use crate::config::model_cards;
use crate::db::Db;
use crate::http::AppState;
use crate::plugins::ArtifactStore;
use crate::routines::RoutineScheduler;
use crate::users::{Shared, UserRegistry, register_session_shards};
use horsie_models::model_cards::ModelCardInput;
use horsie_models::settings::ServerInfo;
use std::path::PathBuf;
use std::sync::Arc;

/// What a host has to decide before a server can exist.
///
/// Deliberately concrete values rather than a config file: parsing is the
/// host's business, and taking a file here would mean every embedder adopts
/// horsie's file format to boot one.
pub struct BootOptions {
    /// Ephemeral state. `server/` beneath it holds the first-boot password
    /// file.
    pub state_dir: PathBuf,
    /// Durable data. `server/` beneath it holds the default SQLite database
    /// and the plugin artifact store.
    pub data_dir: PathBuf,
    /// Anything `sqlx::Any` accepts. Empty means the SQLite file under
    /// `data_dir`.
    pub db_url: Option<String>,
    pub max_connections: u32,
    pub auth_mode: AuthMode,
    /// Extra model cards seeded alongside the bundled catalogue, which every
    /// deployment gets whether or not it asks.
    pub extra_model_cards: Vec<ModelCardInput>,
    /// Built web-UI assets to serve alongside the API.
    pub web_dir: Option<PathBuf>,
    /// Reported in the settings view, purely so an operator can see it.
    pub config_path: Option<PathBuf>,
    /// Where this node reaches the others. Empty means a bus confined to this
    /// process, which is what a single-node deployment wants; a URL is a
    /// deployment saying its nodes have to hear each other.
    pub bus_url: Option<String>,
    /// This node's place in a cluster. `None` is a single-node deployment,
    /// which binds no transport and opens no Raft store.
    pub cluster: Option<crate::cluster::ClusterSection>,
}

impl BootOptions {
    /// Everything defaulted except where the data goes.
    pub fn new(state_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            state_dir,
            data_dir,
            db_url: None,
            max_connections: crate::config::DEFAULT_MAX_CONNECTIONS,
            auth_mode: AuthMode::default(),
            extra_model_cards: Vec::new(),
            web_dir: None,
            config_path: None,
            bus_url: None,
            cluster: None,
        }
    }
}

/// A server that exists but is not listening.
pub struct Booted {
    pub state: AppState,
    /// The generated admin password, present only on the boot that created the
    /// account and only when horsie owns the credential. The caller decides
    /// how to announce it; it is also on disk at
    /// `<state_dir>/server/initial-admin-password`.
    pub initial_password: Option<String>,
    /// `<state_dir>/server`, where that file was written.
    pub state_dir: PathBuf,
}

/// Bring up everything and return it unstarted.
///
/// The order is load-bearing and is the reason this is not a pile of public
/// constructors: the pool comes up first, then the account bootstrap (which
/// needs the pool), and only then anything scoped to an account.
pub async fn boot(opts: BootOptions) -> Result<Booted, String> {
    let state_dir = opts.state_dir.join("server");
    let data_dir = opts.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;

    let db_url = opts
        .db_url
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("sqlite://{}/config.db", data_dir.display()));
    let info = ServerInfo {
        config_path: opts
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        database: redact_db_url(&db_url),
        state_dir: opts.state_dir.display().to_string(),
        data_dir: opts.data_dir.display().to_string(),
        plugins_dir: data_dir.join("plugins").display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let db = Db::open(&db_url, opts.max_connections).await?;

    let auth = Arc::new(AuthService::new(
        AuthStore::new(db.clone()),
        AuthDeps {
            mode: opts.auth_mode,
            state_dir: state_dir.clone(),
        },
    ));
    let initial_password = auth
        .bootstrap()
        .await
        .map_err(|e| format!("bootstrapping the admin account: {e}"))?;

    // The account `Principal::Anonymous` resolves to. Resolved here, once,
    // because `bootstrap` above is what guarantees there is one.
    let anonymous = auth
        .sole_user()
        .await?
        .ok_or_else(|| "no account exists after bootstrap".to_string())?;

    let mut seed = model_cards::bundled_seed()?;
    seed.extend(opts.extra_model_cards);

    // Before anything that could publish. An unusable URL fails the boot
    // rather than falling back to a per-process bus: a node that believes it is
    // clustered and is not looks healthy while losing every cross-node message.
    let bus = crate::bus::open(opts.bus_url.as_deref())
        .await
        .map_err(|e| format!("opening the bus: {e}"))?;

    // Before the system, because `ActorSystem::clustered` takes the node. A
    // refused configuration stops the boot here rather than leaving a node that
    // believes it is clustered and is not.
    let cluster = match &opts.cluster {
        None => None,
        Some(section) => Some(
            crate::cluster::start(&crate::cluster::ClusterInputs {
                section,
                // The *resolved* URL, not what was configured: an absent
                // setting means the SQLite default, which is exactly the case
                // the guard has to refuse.
                database_url: Some(db_url.as_str()),
                bus_url: opts.bus_url.as_deref(),
                state_dir: &opts.state_dir,
            })
            .await
            .map_err(|e| format!("joining the cluster: {e}"))?,
        ),
    };

    let system = crate::users::node_system(&db, cluster.clone());
    if let Some(node) = &cluster {
        // Only now: the pump needs both halves, and nothing may arrive for this
        // node before something is draining it.
        crate::cluster::pump(node, system.clone());
    }

    let shared = Arc::new(Shared {
        system,
        bus,
        db,
        artifacts: Arc::new(ArtifactStore::new(data_dir.join("plugins"))),
        info,
        model_card_seed_marker: model_cards::seed_marker(&seed),
        model_card_seed: Arc::new(seed),
        anonymous,
        supervisor: crate::sessions::supervisor::SupervisorConfig::default(),
        // Only exists in a test build; see `Shared::deps`.
        #[cfg(any(test, feature = "test-util"))]
        deps: None,
        fly_api_base: crate::runtime_vendor::fly_api::DEFAULT_API_BASE.to_string(),
    });
    let users = Arc::new(UserRegistry::new(shared.clone()));
    // Once per node, and only now: a recipe resolves an account's bundle
    // through the registry, so there has to be one for it to close over.
    register_session_shards(&users)?;

    // One timer for the deployment, over every account's routines. It resolves
    // an owner's services when one of their routines comes due, which is also
    // the only thing that builds a bundle nobody has made a request for.
    Arc::new(RoutineScheduler::new(shared.db.clone(), users.clone())).spawn();

    Ok(Booted {
        state: AppState {
            auth,
            shared,
            users,
            web_dir: opts.web_dir,
        },
        initial_password,
        state_dir,
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_database_url_with_a_password_is_redacted() {
        assert_eq!(
            redact_db_url("postgres://u:p@host/db"),
            "postgres://***@host/db"
        );
        assert_eq!(redact_db_url("sqlite:///tmp/x.db"), "sqlite:///tmp/x.db");
    }
}
