//! One project's bundle, and the registry that builds it.
//!
//! [`Shared`] is what a deployment owns: the pool, the content-addressed
//! artifact store, and the values resolved once at boot.
//! [`ProjectServices`] is what a project owns — its session supervisor and
//! every actor beneath it, its event channel, its vendor map, its provider
//! clients, and every scoped store.
//!
//! Nothing here filters. Two projects do not share a map with a `project_id` on
//! the key or a channel with a `project_id` on the frame; they hold different
//! objects, which is a boundary a forgotten line cannot cross. That is as true
//! of two projects belonging to the *same* account as of two accounts.

use crate::auth::UserId;
use crate::config::{DbConfigStore, StoreDeps, model_cards};
use crate::db::Db;
use crate::db::journal::SqlJournal;
use crate::plugins::ArtifactStore;
use crate::projects::{ProjectId, ProjectStore};
use crate::sessions::SessionRevisions;
use crate::sessions::addressing::{SessionShard, SupervisorRef, SupervisorShard};
use crate::sessions::session_actor::SessionActor;
use crate::sessions::spec::ServerDeps;
use crate::sessions::spec::{RuntimeVendorMap, SharedProviderRegistry};
use crate::sessions::supervisor::{SessionSupervisor, SessionSupervisorCommand, SupervisorConfig};
use horsie_actor::{ActorSystem, Journal};
use horsie_models::model_cards::ModelCardInput;
use horsie_models::settings::ServerInfo;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::OnceCell;

/// The actor system this process runs, over the node's journal.
///
/// One per node rather than one per account. A clustered node has a single
/// inbound channel, so a process holding a system per account would have nothing
/// to route an arriving envelope by — and the journal stopped being scoped when
/// a log became findable by `(kind, id)` alone, so there is no second reason for
/// the split either. Accounts stay apart because their actors sit at different
/// paths, which is what a path was always for.
///
/// `cluster` is `None` on a single-node deployment, which is the default: every
/// address is then local and this node owns everything.
#[must_use]
pub fn node_system(db: &Db, cluster: Option<Arc<horsie_actor::ClusterNode>>) -> ActorSystem {
    let journal = Arc::new(SqlJournal::new(db.clone())) as Arc<dyn Journal>;
    match cluster {
        Some(node) => ActorSystem::clustered(journal, node),
        None => ActorSystem::new(journal),
    }
}

/// Everything one deployment owns, whatever accounts it serves.
pub struct Shared {
    /// One pool. Every account's stores bind their scope into its queries.
    pub db: Db,
    /// Every actor this node hosts, for every account. See [`node_system`].
    pub system: ActorSystem,
    /// Whether this node may still act on the actors it hosts.
    ///
    /// `None` unclustered. On a clustered node it goes false while the node
    /// cannot see a leader, and every account's supervisor reference reads it
    /// before sending.
    pub serving: crate::sessions::addressing::Serving,
    /// Where a frame goes to reach another node.
    ///
    /// One per deployment, not one per account: a topic name already carries
    /// the account, and a bus per account would mean a connection per account
    /// to whatever backs it. Isolation between accounts is the topic namespace,
    /// exactly as it is for actors and their addresses.
    pub bus: Arc<dyn crate::bus::Bus>,
    /// Bundle bytes, addressed by content and therefore shared by construction.
    pub artifacts: Arc<ArtifactStore>,
    /// The `projects` table: who owns which scope, and how one is created or
    /// destroyed.
    ///
    /// Deployment-wide rather than per project, because it is what *answers*
    /// which project a request may use — a per-project copy would have to exist
    /// before the question it settles.
    pub project_service: Arc<crate::projects::ProjectService>,
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
    /// What every account's session actors run on, when a harness has already
    /// assembled it.
    ///
    /// Compiled out of a deployment entirely, and that is the point rather than
    /// tidiness: setting it replaces *every* account's clients with one bundle,
    /// which is the per-account boundary this module opens by describing
    /// collapsing in a single line. A test has one account and cannot notice; a
    /// deployment would, silently. It exists at all because a shard recipe has
    /// nowhere else to take a fake runtime vendor from.
    #[cfg(any(test, feature = "test-util"))]
    pub deps: Option<ServerDeps>,
    /// The Fly Machines API root every account's Fly vendors are built against.
    ///
    /// Deployment-wide because it is a property of where this server runs, not
    /// of who is configuring a vendor: a server inside Fly's own network can
    /// use the internal endpoint. It is also the seam a test points somewhere
    /// harmless, since saving a Fly vendor now calls this API.
    pub fly_api_base: String,
    /// The GitHub REST root every account's GitHub service is built against.
    ///
    /// Deployment-wide for the same reason as the Fly root, and injectable for
    /// a sharper one: minting an installation token is the last step of the
    /// runtime credential route, so a test that cannot point it at a mock can
    /// only ever assert refusals — which is how a route that refused *every*
    /// mint passed its whole suite.
    pub github_api_base: String,
}

/// Everything one project owns.
pub struct ProjectServices {
    pub project: ProjectId,
    /// Who may reach this project. Read by [`crate::http::Scope`], which is the
    /// only thing left in the system that asks a question about a *user* rather
    /// than about a project.
    pub owner: UserId,
    /// This project's session list, addressed rather than held. Resolving it
    /// starts nothing: the supervisor comes into being on the first command
    /// sent through this, on whichever node the cluster puts it.
    pub supervisor: SupervisorRef,
    /// What this account's session and agent actors run on: its runtime
    /// manager, its provider clients, its plugin library.
    ///
    /// Here rather than passed at construction because a session is built by a
    /// shard recipe now, from an id and nothing else — so an actor reaches its
    /// wiring by resolving this bundle rather than by being handed it.
    pub deps: ServerDeps,
    /// Where readers wait for this account's sessions to move.
    pub revisions: Arc<SessionRevisions>,
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
    pub authored: Arc<crate::plugins::authored::AuthoredService>,
    pub memory: Arc<crate::memory::MemoryService>,
    pub agents: Arc<crate::agents::AgentService>,
    /// The agent-run index. A store rather than a service: the session actor
    /// writes rows it derived from its own state, and the control plane reads
    /// them straight back.
    pub agent_runs: Arc<crate::agent_runs::AgentRunStore>,
    /// The person's inbox for this project: what its agents have said to them,
    /// and what they are parked on waiting to hear back. A store rather than a
    /// service, like `agent_runs` beside it and for the same reason.
    pub user_inbox: Arc<crate::user_inbox::UserInboxStore>,
    /// Version history and compare-and-set for presets and memories.
    ///
    /// `entity_revisions`, not `revisions`: the field of that name beside it is
    /// the live counter an SSE reader watches, which is a different thing that
    /// happens to share a word.
    pub entity_revisions: Arc<crate::revisions::RevisionStore>,
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
    /// Signs this account's dial-back tokens. See [`OpenedConfig::dial_secret`].
    ///
    /// [`OpenedConfig::dial_secret`]: crate::config::store::OpenedConfig::dial_secret
    pub dial_secret: Arc<Vec<u8>>,
}

/// Build one project's services on the shared deployment tier.
///
/// Lifted from what `main` used to do exactly once. Ordering matters in two
/// places now: the row is read first, and the config store opens second because
/// it builds the provider registry and the (empty) vendor map everything below
/// reads.
///
/// The row comes first for two reasons that are really one — it is where the
/// owner comes from, and a project with no row must not produce a bundle. An
/// actor recovering on another node resolves its wiring from an id and nothing
/// else, so this is the only place either can be established.
async fn build_project(
    project: ProjectId,
    shared: &Shared,
) -> Result<Arc<ProjectServices>, String> {
    let owner = ProjectStore::new(shared.db.clone())
        .get(&project)
        .await?
        .ok_or_else(|| format!("no such project: {project}"))?
        .user_id;

    let opened = DbConfigStore::open_on(
        shared.db.clone(),
        StoreDeps {
            info: shared.info.clone(),
        },
        project.clone(),
    )
    .await?;

    let model_cards = Arc::new(model_cards::ModelCardStore::new(
        shared.db.clone(),
        project.clone(),
    ));
    // A database failure here warns rather than failing the build: an account
    // with an unseeded catalogue still works, and the admin API is what fixes
    // the state. Refusing to build the bundle would take the whole account down
    // over reference data.
    if let Err(e) = model_cards
        .seed_once(&shared.model_card_seed, &shared.model_card_seed_marker)
        .await
    {
        tracing::warn!(project = %project, error = ?e, "seeding model cards failed");
    }

    let github = Arc::new(crate::github::GithubService::new(
        crate::github::GithubStore::new(shared.db.clone(), project.clone()),
        crate::github::GithubApi::with_bases(
            crate::github::GITHUB_WEB_BASE,
            &shared.github_api_base,
        ),
    ));
    let mcp = Arc::new(crate::mcp::McpService::new(
        crate::mcp::McpStore::new(shared.db.clone(), project.clone()),
        github.clone(),
    ));
    let plugins = Arc::new(crate::plugins::PluginService::new(
        crate::plugins::PluginStore::new(shared.db.clone(), project.clone()),
        crate::plugins::MarketplaceStore::new(shared.db.clone(), project.clone()),
        shared.artifacts.clone(),
        crate::plugins::authored::AuthoredStore::new(shared.db.clone(), project.clone()),
    ));
    let authored = Arc::new(crate::plugins::authored::AuthoredService::new(
        crate::plugins::authored::AuthoredStore::new(shared.db.clone(), project.clone()),
        plugins.clone(),
    ));
    let memory = Arc::new(crate::memory::MemoryService::new(
        crate::memory::MemoryStore::new(shared.db.clone(), project.clone()),
    ));
    let agents = Arc::new(crate::agents::AgentService::new(
        crate::agents::AgentStore::new(shared.db.clone(), project.clone()),
        opened.store.clone(),
    ));
    let entity_revisions = Arc::new(crate::revisions::RevisionStore::new(
        shared.db.clone(),
        project.clone(),
    ));
    let agent_runs = Arc::new(crate::agent_runs::AgentRunStore::new(
        shared.db.clone(),
        project.clone(),
    ));
    let user_inbox = Arc::new(crate::user_inbox::UserInboxStore::new(
        shared.db.clone(),
        project.clone(),
    ));
    let routines = Arc::new(crate::routines::RoutineService::new(
        crate::routines::RoutineStore::new(shared.db.clone(), project.clone()),
        agents.clone(),
    ));
    let environments = Arc::new(crate::environments::EnvironmentService::new(
        crate::environments::EnvironmentStore::new(shared.db.clone(), project.clone()),
    ));
    let workflows = Arc::new(crate::workflows::WorkflowService::new(
        crate::workflows::WorkflowStore::new(shared.db.clone(), project.clone()),
        agents.clone(),
    ));
    let chatgpt = Arc::new(crate::config::chatgpt_login::ChatGptLoginService::new(
        shared.db.clone(),
        project.clone(),
        opened.store.clone(),
    ));

    let connected_vendors = Arc::new(crate::runtime_vendor::RuntimeVendorRegistry::new(
        opened.vendors.clone(),
    ));
    let runtime_vendors = Arc::new(
        crate::runtime_vendor::RuntimeVendorConfigService::new(
            crate::runtime_vendor::RuntimeVendorStore::new(shared.db.clone(), project.clone()),
            opened.vendors.clone(),
            // The registry's own table, so the two publishers of one map can see
            // each other's names rather than silently overwriting them.
            connected_vendors.links(),
        )
        .with_fly_api_base(shared.fly_api_base.clone()),
    );
    // Before anything can select a vendor: a session started early would
    // otherwise be told its configured vendor does not exist.
    runtime_vendors.publish_all().await;
    let runtimes = Arc::new(crate::runtime_manager::RuntimeManager::new(
        crate::runtime_manager::RuntimeDeps {
            vendors: opened.vendors.clone(),
            github_tokens: Some(github.clone()),
            plugins: Some(plugins.clone() as Arc<dyn crate::plugins::PluginProvisioner>),
            dial_secret: opened.dial_secret.clone(),
            account: project.as_str().to_string(),
            bus: shared.bus.clone(),
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
    #[cfg(any(test, feature = "test-util"))]
    let deps = shared.deps.clone().unwrap_or(deps);

    // Named, not started. A shard type's actors are built by the recipe this
    // node registered, on whichever node the cluster placed them — so building
    // an account's bundle no longer starts anything, and the supervisor comes
    // into being when the first command is addressed to it.
    let supervisor = SupervisorRef::new(
        shared.system.shard_actor_of::<SupervisorShard>(),
        project.clone(),
        shared.serving.clone(),
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
        let project = project.clone();
        tokio::spawn(async move {
            let Ok(sessions) = supervisor
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
            else {
                tracing::warn!(project = %project, "orphan sweep skipped: the session list is unreadable");
                return;
            };
            let live = sessions.into_iter().map(|(id, _)| id).collect();
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

    // The same account form the runtime topics use, because both name topics on
    // the one bus this deployment shares.
    let account = project.as_str().to_string();
    Ok(Arc::new(ProjectServices {
        authored,
        project,
        owner,
        supervisor,
        deps,
        revisions: Arc::new(SessionRevisions::new(&account, shared.bus.clone())),
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
        agent_runs,
        user_inbox,
        entity_revisions,
        routines,
        routine_runner,
        environments,
        workflows,
        connected_vendors,
        runtime_vendors,
        dial_secret: opened.dial_secret.clone(),
    }))
}

/// Every project's services, built on first touch and kept for the process
/// lifetime.
///
/// Not unloaded when idle, deliberately. A bundle is a handful of `Arc`s, one
/// addressed actor and one channel, and the supervisor already unloads its own
/// idle sessions — so a dormant project costs about what a dormant account cost
/// before projects existed. An account with several of them therefore costs
/// several of those, which is the price of the isolation being structural
/// rather than filtered. Unloading a bundle whose session is mid-turn is a bug
/// worth not inventing before anything has measured the need for it.
pub struct ProjectRegistry {
    shared: Arc<Shared>,
    /// The `OnceCell` is load-bearing, not tidiness. A bundle is what a
    /// project's actors resolve their wiring from, and two concurrent first
    /// requests that each ran `build_project` would leave the project with two of
    /// them — two runtime managers over one set of sandboxes, two vendor maps,
    /// two event channels, and a reader watching whichever one it happened to
    /// resolve. The write lock is taken only to insert the empty cell, so the
    /// build itself never runs under it.
    projects: RwLock<HashMap<ProjectId, Arc<OnceCell<Arc<ProjectServices>>>>>,
}

/// One project's bundle, for an actor resolving its own wiring at recovery.
///
/// The failure branches are logged rather than handled because neither is
/// reachable from a running deployment: a reference to one of this account's
/// actors is only ever taken *out of* its bundle, so a caller that could not
/// build one never had anything to address. What is left is a process on its
/// way down, whose registry has already gone.
pub async fn resolve(
    projects: &std::sync::Weak<ProjectRegistry>,
    project: &ProjectId,
) -> Option<Arc<ProjectServices>> {
    let Some(registry) = projects.upgrade() else {
        tracing::warn!(project = %project, "the account registry is gone; the process is shutting down");
        return None;
    };
    match registry.get(project).await {
        Ok(services) => Some(services),
        Err(e) => {
            tracing::error!(project = %project, error = %e, "could not resolve the account's services");
            None
        }
    }
}

/// Teach this node how to build an account's supervisor and its sessions.
///
/// Called once per node, after the registry exists, because that is what a
/// recipe resolves an account's bundle through. A `Weak` and not an `Arc`: the
/// registry holds [`Shared`], which holds the system, which would then hold the
/// recipe — a cycle nothing ever collects.
///
/// A recipe is synchronous and infallible, so neither of these resolves a
/// bundle here. Each actor does that in `on_recovery_complete`, which is async
/// and runs before any command it is sent.
pub fn register_session_shards(projects: &Arc<ProjectRegistry>) -> Result<(), String> {
    let system = &projects.shared().system;
    let config = projects.shared().supervisor.clone();
    let registry = Arc::downgrade(projects);
    system
        .shard::<SupervisorShard>()
        .register(move |system, entity| {
            system.persistent(SessionSupervisor::new(
                entity.entity_id.clone(),
                registry.clone(),
                config.clone(),
            ))
        })
        .map_err(|e| format!("could not register the session supervisor: {e}"))?;

    let registry = Arc::downgrade(projects);
    system
        .shard::<SessionShard>()
        .register(move |system, entity| {
            // The supervisor is reached as a *name* for its whole type, so a
            // session built on a host that never saw the request creating it
            // still has one to report to.
            // Ungated on purpose: this reference is held by a session actor,
            // and a node that stands down has its hosted actors stopped for it.
            // The gate belongs on the reference a *request* arrives through.
            let supervisor = SupervisorRef::new(
                system.shard_actor_of::<SupervisorShard>(),
                entity.entity_id.project.clone(),
                None,
            );
            system.persistent(SessionActor::new(
                entity.entity_id.clone(),
                supervisor,
                registry.clone(),
            ))
        })
        .map_err(|e| format!("could not register the session type: {e}"))
}

impl ProjectRegistry {
    #[must_use]
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            projects: RwLock::new(HashMap::new()),
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
    pub fn is_built(&self, project: &ProjectId) -> bool {
        self.projects
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(project)
    }

    /// This project's services, building them if this is its first touch.
    ///
    /// **Get-or-*build*, not get-or-create.** An id with no `projects` row is an
    /// error rather than a fresh bundle, so a stranger cannot spawn a supervisor
    /// by naming one. That is necessary and not sufficient: an id belonging to
    /// *another account* is real, so a caller must also have checked ownership —
    /// which [`crate::http::Scope`] does, before it gets here.
    pub async fn get(&self, project: &ProjectId) -> Result<Arc<ProjectServices>, String> {
        let cell = {
            let mut projects = self
                .projects
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            projects.entry(project.clone()).or_default().clone()
        };
        cell.get_or_try_init(|| build_project(project.clone(), &self.shared))
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

    async fn registry(tmp: &tempfile::TempDir) -> Arc<ProjectRegistry> {
        let db = crate::db::testing::db().await;
        let users = Arc::new(ProjectRegistry::new(Arc::new(Shared {
            bus: Arc::new(crate::bus::MemoryBus::new()),
            system: node_system(&db, None),
            serving: None,
            project_service: Arc::new(crate::projects::ProjectService::new(db.clone())),
            db,
            artifacts: Arc::new(ArtifactStore::new(tmp.path().join("artifacts"))),
            info: test_info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: model_cards::seed_marker(&[]),
            anonymous: UserId::bootstrap(),
            supervisor: SupervisorConfig::default(),
            deps: None,
            fly_api_base: crate::testing::UNREACHABLE_FLY_API.to_string(),
            github_api_base: crate::testing::UNREACHABLE_GITHUB_API.to_string(),
        })));
        register_session_shards(&users).expect("a fresh system has no shard types yet");
        users
    }

    /// A real project, of its own account.
    ///
    /// A bare `ProjectId::generate()` is not enough any more, and that is the
    /// point: an id with no row builds no bundle, so a stranger cannot spawn a
    /// supervisor by naming one.
    async fn a_project(reg: &ProjectRegistry) -> ProjectId {
        reg.shared()
            .project_service
            .default_project(&UserId::generate())
            .await
            .expect("a fresh account gets a default project")
            .id
    }

    /// The assertion the `OnceCell` exists for. Two callers racing a project's
    /// first request must get the same bundle — a second one would mean a
    /// second `SessionSupervisor` on the same persistence id, two
    /// event-sourced actors writing one journal.
    #[tokio::test]
    async fn concurrent_first_touches_build_one_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let project = a_project(&reg).await;

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let reg = reg.clone();
            let project = project.clone();
            tasks.push(tokio::spawn(
                async move { reg.get(&project).await.unwrap() },
            ));
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

    /// Two accounts' supervisors are two actors on the *one* system this node
    /// runs, kept apart by their addresses.
    ///
    /// The node has a single inbound channel when it is clustered, so a process
    /// holding a system per account would have nothing to route an arriving
    /// envelope by. What replaced that separation is what an address was always
    /// for: two accounts are two entities of one shard type on one registry.
    ///
    /// Resolving a bundle deliberately starts nothing — the first command is
    /// what builds the actor — so the count is taken after each account has
    /// been spoken to.
    #[tokio::test]
    async fn every_account_is_hosted_on_the_one_node_system() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let before = reg.shared().system.hosted();

        let (a, b) = (a_project(&reg).await, a_project(&reg).await);
        for project in [&a, &b] {
            reg.get(project)
                .await
                .unwrap()
                .supervisor
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
                .unwrap();
        }

        assert_eq!(
            reg.shared().system.hosted(),
            before + 2,
            "both supervisors must land on the node's system, not on one apiece"
        );
    }

    /// Two projects share the pool and nothing above it — whether or not they
    /// belong to the same account.
    #[tokio::test]
    async fn two_projects_get_separate_supervisors_and_channels() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let (a, b) = (a_project(&reg).await, a_project(&reg).await);
        let (sa, sb) = (reg.get(&a).await.unwrap(), reg.get(&b).await.unwrap());

        assert!(!Arc::ptr_eq(&sa, &sb));
        // One account's list moving is invisible to the other, because there is
        // no path between them to filter. Two counters, not one counter and a
        // scope check — a filter is one forgotten line from leaking a title.
        let b_before = *sb.revisions.list().borrow();
        sa.revisions.bump_list();
        assert_eq!(
            *sb.revisions.list().borrow(),
            b_before,
            "one account's session list must not move another's"
        );
        assert_ne!(
            *sa.revisions.list().borrow(),
            b_before,
            "and the bump must have actually happened"
        );
    }

    /// A dormant project costs nothing until something asks for it.
    #[tokio::test]
    async fn an_untouched_project_is_never_built() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry(&tmp).await;
        let project = a_project(&reg).await;
        assert!(reg.projects.read().unwrap().is_empty());
        let _ = reg.get(&project).await.unwrap();
        assert_eq!(reg.projects.read().unwrap().len(), 1);
    }
}
