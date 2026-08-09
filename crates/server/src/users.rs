//! One account's world, and the registry that builds it.
//!
//! [`Shared`] is what a deployment owns: the pool, the content-addressed
//! artifact store, and the values resolved once at boot.
//! [`UserServices`] is what an account owns — its session supervisor and every
//! actor beneath it, its journal, its event channel, its vendor map, its
//! provider clients, and every scoped store.
//!
//! Nothing here filters. Two accounts do not share a map with a `user_id` on
//! the key or a channel with a `user_id` on the frame; they hold different
//! objects, which is a boundary a forgotten line cannot cross.

use crate::auth::UserId;
use crate::config::{DbConfigStore, StoreDeps, model_cards};
use crate::db::Db;
use crate::db::journal::SqlJournal;
use crate::plugins::ArtifactStore;
use crate::sessions::spec::ServerDeps;
use crate::sessions::spec::{RuntimeVendorMap, SharedProviderRegistry};
use crate::sessions::supervisor::{SessionSupervisor, SessionSupervisorCommand, SupervisorConfig};
use horsie_actor::{ActorRef, Journal, spawn_root};
use horsie_models::model_cards::ModelCardInput;
use horsie_models::session::GlobalSessionEvent;
use horsie_models::settings::ServerInfo;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{OnceCell, broadcast};

/// How many global events a subscriber may fall behind before it is lagged.
const GLOBAL_EVENT_CAPACITY: usize = 256;

/// Everything one deployment owns, whatever accounts it serves.
pub struct Shared {
    /// One pool. Every account's stores bind their scope into its queries.
    pub db: Db,
    /// Bundle bytes, addressed by content and therefore shared by construction.
    pub artifacts: Arc<ArtifactStore>,
    /// Read-only deployment paths, surfaced in every account's settings view.
    pub info: ServerInfo,
    /// The model-card catalogue every account is seeded from, plus the digest
    /// that says whether it already has been. Resolved once, at boot.
    pub model_card_seed: Arc<Vec<ModelCardInput>>,
    pub model_card_seed_marker: String,
    /// The account `Principal::Anonymous` resolves to — which is every request
    /// on a deployment with authentication disabled.
    pub anonymous: UserId,
    /// The idle policy every account's supervisor is built with. Deployment
    /// policy, not account preference — and the seam a test drives time
    /// through.
    pub supervisor: SupervisorConfig,
}

/// Everything one account owns.
pub struct UserServices {
    pub user: UserId,
    /// This account's session list, and the parent of every session and agent
    /// actor it has loaded.
    pub supervisor: ActorRef<SessionSupervisorCommand>,
    /// This account's global event stream. One channel per account rather than
    /// one channel and a filter: a filter is one forgotten line from leaking
    /// every session title on the server.
    pub global_events: broadcast::Sender<GlobalSessionEvent>,
    pub config_store: Arc<dyn crate::config::ConfigStore>,
    /// This account's live LLM clients, keyed by model alias, and the vendor
    /// map its sessions select a runtime from. Both are handles the config
    /// store, the supervisor and the runtime manager already share; held here
    /// so a caller that needs to look at one does not have to reach through a
    /// service that happens to own it.
    pub provider_registry: SharedProviderRegistry,
    pub vendors: RuntimeVendorMap,
    pub model_cards: Arc<model_cards::ModelCardStore>,
    pub chatgpt: Arc<crate::config::chatgpt_login::ChatGptLoginService>,
    pub github: Arc<crate::github::GithubService>,
    pub mcp: Arc<crate::mcp::McpService>,
    pub plugins: Arc<crate::plugins::PluginService>,
    pub memory: Arc<crate::memory::MemoryService>,
    pub agents: Arc<crate::agents::AgentService>,
    pub routines: Arc<crate::routines::RoutineService>,
    pub routine_runner: Arc<crate::routines::RoutineRunner>,
    pub environments: Arc<crate::environments::EnvironmentService>,
    pub workflows: Arc<crate::workflows::WorkflowService>,
    /// Where this account's vendor processes publish themselves. A name claimed
    /// here is claimed for this account only, so `main` is available to
    /// everyone — and no session can select another account's runtime, because
    /// it is not in the map it reads.
    pub connected_vendors: Arc<crate::runtime_vendor::RuntimeVendorRegistry>,
    /// This account's runtime vendors that are *configured* rather than dialled
    /// in. A cloud vendor has nothing to announce itself from, so its row is
    /// the only record it exists; this service is what rebuilds it at boot.
    pub runtime_vendors: Arc<crate::runtime_vendor::RuntimeVendorConfigService>,
    /// Where this account's runtimes land when they dial `/api/runtime/connect`.
    ///
    /// One per account rather than one per server: the dial token names the
    /// account, so there is no lookup to get wrong, and a transport can never
    /// be resolved across accounts even if two of them somehow shared a
    /// runtime id.
    pub connected_runtimes: Arc<horsie_runtime_host::ConnectedRuntimeRegistry>,
    /// Signs this account's dial-back tokens. See [`OpenedConfig::dial_secret`].
    ///
    /// [`OpenedConfig::dial_secret`]: crate::config::store::OpenedConfig::dial_secret
    pub dial_secret: Arc<Vec<u8>>,
}

/// Build one account's services on the shared deployment tier.
///
/// Lifted from what `main` used to do exactly once. Ordering matters in one
/// place: the config store opens first because it builds the provider registry
/// and the (empty) vendor map everything below reads.
async fn build_user(user: UserId, shared: &Shared) -> Result<Arc<UserServices>, String> {
    let opened = DbConfigStore::open_on(
        shared.db.clone(),
        StoreDeps {
            info: shared.info.clone(),
        },
        user.clone(),
    )
    .await?;

    let model_cards = Arc::new(model_cards::ModelCardStore::new(
        shared.db.clone(),
        user.clone(),
    ));
    // A database failure here warns rather than failing the build: an account
    // with an unseeded catalogue still works, and the admin API is what fixes
    // the state. Refusing to build the bundle would take the whole account down
    // over reference data.
    if let Err(e) = model_cards
        .seed_once(&shared.model_card_seed, &shared.model_card_seed_marker)
        .await
    {
        tracing::warn!(user = %user, error = ?e, "seeding model cards failed");
    }

    let github = Arc::new(crate::github::GithubService::new(
        crate::github::GithubStore::new(shared.db.clone(), user.clone()),
        crate::github::GithubApi::new(),
    ));
    let mcp = Arc::new(crate::mcp::McpService::new(
        crate::mcp::McpStore::new(shared.db.clone(), user.clone()),
        github.clone(),
    ));
    let plugins = Arc::new(crate::plugins::PluginService::new(
        crate::plugins::PluginStore::new(shared.db.clone(), user.clone()),
        crate::plugins::MarketplaceStore::new(shared.db.clone(), user.clone()),
        shared.artifacts.clone(),
    ));
    let memory = Arc::new(crate::memory::MemoryService::new(
        crate::memory::MemoryStore::new(shared.db.clone(), user.clone()),
    ));
    let agents = Arc::new(crate::agents::AgentService::new(
        crate::agents::AgentStore::new(shared.db.clone(), user.clone()),
        opened.store.clone(),
    ));
    let routines = Arc::new(crate::routines::RoutineService::new(
        crate::routines::RoutineStore::new(shared.db.clone(), user.clone()),
        agents.clone(),
    ));
    let environments = Arc::new(crate::environments::EnvironmentService::new(
        crate::environments::EnvironmentStore::new(shared.db.clone(), user.clone()),
    ));
    let workflows = Arc::new(crate::workflows::WorkflowService::new(
        crate::workflows::WorkflowStore::new(shared.db.clone(), user.clone()),
        agents.clone(),
    ));
    let chatgpt = Arc::new(crate::config::chatgpt_login::ChatGptLoginService::new(
        shared.db.clone(),
        user.clone(),
        opened.store.clone(),
    ));

    let connected_vendors = Arc::new(crate::runtime_vendor::RuntimeVendorRegistry::new(
        opened.vendors.clone(),
    ));
    let connected_runtimes = Arc::new(horsie_runtime_host::ConnectedRuntimeRegistry::new());
    let runtime_vendors = Arc::new(crate::runtime_vendor::RuntimeVendorConfigService::new(
        crate::runtime_vendor::RuntimeVendorStore::new(shared.db.clone(), user.clone()),
        opened.vendors.clone(),
        // The registry's own table, so the two publishers of one map can see
        // each other's names rather than silently overwriting them.
        connected_vendors.links(),
        connected_runtimes.clone(),
    ));
    // Before anything can select a vendor: a session started early would
    // otherwise be told its configured vendor does not exist.
    runtime_vendors.publish_all().await;
    let runtimes = Arc::new(crate::runtime_manager::RuntimeManager::new(
        crate::runtime_manager::RuntimeDeps {
            vendors: opened.vendors.clone(),
            github_tokens: Some(github.clone()),
            plugins: Some(plugins.clone() as Arc<dyn crate::plugins::PluginProvisioner>),
            dial_secret: opened.dial_secret.clone(),
            account: user.as_str().to_string(),
        },
    ));
    let deps = ServerDeps {
        runtimes,
        provider_registry: opened.registry.clone(),
        vendors: opened.vendors.clone(),
        github_tokens: Some(github.clone()),
        mcp: Some(mcp.clone()),
        plugins: Some(plugins.clone() as Arc<dyn crate::plugins::PluginProvisioner>),
        memory: Some(memory.clone()),
    };

    let journal: Arc<dyn Journal> = Arc::new(SqlJournal::new(shared.db.clone(), user.clone()));
    let (global_events, _) = broadcast::channel(GLOBAL_EVENT_CAPACITY);
    let supervisor = spawn_root(
        SessionSupervisor::with_config(
            user.clone(),
            deps,
            global_events.clone(),
            shared.supervisor.clone(),
        ),
        journal,
    );

    // Destroy substrate left over from sessions that no longer exist. Deleting a
    // session already tells its vendor; this covers the case where the vendor
    // was unreachable at the time, and the machine has been billing ever since.
    //
    // Detached, because it makes network calls and an account's first request
    // must not wait on a cloud API. Skipped entirely if the session list cannot
    // be read: a partial list would mark live runtimes as orphans and destroy
    // their workspaces.
    {
        let supervisor = supervisor.clone();
        let runtime_vendors = runtime_vendors.clone();
        let user = user.clone();
        tokio::spawn(async move {
            let Ok(sessions) = supervisor
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
            else {
                tracing::warn!(user = %user, "orphan sweep skipped: the session list is unreadable");
                return;
            };
            let live = sessions.into_iter().map(|(id, _, _)| id).collect();
            runtime_vendors.sweep_orphans(&live).await;
        });
    }

    let routine_runner = Arc::new(crate::routines::RoutineRunner::new(
        routines.clone(),
        agents.clone(),
        environments.clone(),
        opened.store.clone(),
        connected_vendors.clone(),
        supervisor.clone(),
    ));

    Ok(Arc::new(UserServices {
        user,
        supervisor,
        global_events,
        config_store: opened.store,
        provider_registry: opened.registry,
        vendors: opened.vendors,
        model_cards,
        chatgpt,
        github,
        mcp,
        plugins,
        memory,
        agents,
        routines,
        routine_runner,
        environments,
        workflows,
        connected_vendors,
        runtime_vendors,
        connected_runtimes,
        dial_secret: opened.dial_secret.clone(),
    }))
}

/// Every account's services, built on first touch and kept for the process
/// lifetime.
///
/// Not unloaded when idle, deliberately. A bundle is a handful of `Arc`s, one
/// actor and one channel, and the supervisor already unloads its own idle
/// sessions — so a dormant account costs about what a dormant deployment cost
/// before this existed. Unloading a bundle whose session is mid-turn is a bug
/// worth not inventing before anything has measured the need for it.
pub struct UserRegistry {
    shared: Arc<Shared>,
    /// The `OnceCell` is load-bearing, not tidiness. Two concurrent first
    /// requests that each ran `build_user` would `spawn_root` two
    /// `SessionSupervisor`s on one persistence id — two event-sourced actors
    /// writing one journal. The write lock is taken only to insert the empty
    /// cell, so the build itself never runs under it.
    users: RwLock<HashMap<UserId, Arc<OnceCell<Arc<UserServices>>>>>,
}

impl UserRegistry {
    #[must_use]
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            users: RwLock::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Whether this account has been touched, without touching it.
    ///
    /// For assertions about what a request *did not* build: every other way of
    /// asking would build the thing being asked about.
    #[must_use]
    pub fn is_built(&self, user: &UserId) -> bool {
        self.users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(user)
    }

    /// This account's services, building them if this is its first touch.
    ///
    /// **Get-or-create.** Every caller has to have established that the account
    /// is real first — a request that reaches here with an unverified,
    /// caller-supplied id is a way to spawn a supervisor per stranger.
    pub async fn get(&self, user: &UserId) -> Result<Arc<UserServices>, String> {
        let cell = {
            let mut users = self
                .users
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            users.entry(user.clone()).or_default().clone()
        };
        cell.get_or_try_init(|| build_user(user.clone(), &self.shared))
            .await
            .cloned()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn test_info() -> ServerInfo {
        ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        }
    }

    async fn registry(tmp: &tempfile::TempDir) -> UserRegistry {
        let db = crate::db::testing::db().await;
        UserRegistry::new(Arc::new(Shared {
            db,
            artifacts: Arc::new(ArtifactStore::new(tmp.path().join("artifacts"))),
            info: test_info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: model_cards::seed_marker(&[]),
            anonymous: UserId::bootstrap(),
            supervisor: SupervisorConfig::default(),
        }))
    }

    /// The assertion the `OnceCell` exists for. Two callers racing an account's
    /// first request must get the same bundle — a second one would mean a
    /// second `SessionSupervisor` on the same persistence id, two
    /// event-sourced actors writing one journal.
    #[tokio::test]
    async fn concurrent_first_touches_build_one_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry(&tmp).await);
        let user = UserId::generate();

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let reg = reg.clone();
            let user = user.clone();
            tasks.push(tokio::spawn(async move { reg.get(&user).await.unwrap() }));
        }
        let mut built = Vec::new();
        for t in tasks {
            built.push(t.await.unwrap());
        }
        for other in &built[1..] {
            assert!(
                Arc::ptr_eq(&built[0], other),
                "an account's bundle must be built exactly once"
            );
        }
    }

    /// Two accounts share the pool and nothing above it.
    #[tokio::test]
    async fn two_accounts_get_separate_supervisors_and_channels() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let (a, b) = (UserId::generate(), UserId::generate());
        let (sa, sb) = (reg.get(&a).await.unwrap(), reg.get(&b).await.unwrap());

        assert!(!Arc::ptr_eq(&sa, &sb));
        // A frame published to one account's stream reaches nobody on the
        // other's, because there is no path between them to filter.
        let mut watching_b = sb.global_events.subscribe();
        sa.global_events
            .send(GlobalSessionEvent::TitleChanged(
                horsie_models::session::GlobalSessionTitleEvent {
                    session_id: "x".into(),
                    name: "a's session".into(),
                },
            ))
            .ok();
        assert!(watching_b.try_recv().is_err());
    }

    /// A dormant account costs nothing until something asks for it.
    #[tokio::test]
    async fn an_untouched_account_is_never_built() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let user = UserId::generate();
        assert!(reg.users.read().unwrap().is_empty());
        let _ = reg.get(&user).await.unwrap();
        assert_eq!(reg.users.read().unwrap().len(), 1);
    }
}
