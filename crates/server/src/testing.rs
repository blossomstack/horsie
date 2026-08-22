// Test scaffolding, not production code: a composition root that will not
// build is a broken test environment, and failing loudly where it fails beats
// threading a Result through every caller.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The real composition root, on a throwaway deployment.
//!
//! Every suite that drives horsie over HTTP needs the same thing: a `Shared`, a
//! `ProjectRegistry`, an `AuthService`, and an `AppState` wired together the way
//! `boot` wires them. Four suites used to assemble that by hand, which meant
//! four places to update when the root gained a field and four slightly
//! different deployments to reason about when one of them failed alone.
//!
//! Deliberately built *through* [`ProjectRegistry`] rather than by assembling a
//! bundle directly: what these tests exercise is what a request actually
//! resolves, including the lazy per-account build.

use crate::auth::{AuthDeps, AuthMode, AuthService, AuthStore, UserId};
use crate::db::Db;
use crate::http::AppState;
use crate::plugins::ArtifactStore;
use crate::projects::{
    ProjectId, ProjectRegistry, ProjectServices, Shared, register_session_shards,
};
use crate::sessions::supervisor::SupervisorConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// A built deployment: the state a request resolves through, the account it
/// resolves to when nothing else says otherwise, and that account's default
/// project.
pub struct TestState {
    pub state: AppState,
    /// The bootstrap account. Unauthenticated requests resolve here, and it is
    /// the one a single-account suite means by "the user".
    pub user: UserId,
    /// [`Self::user`]'s default project: the scope every URL in a
    /// single-project suite carries, and what [`Self::url`] builds a path from.
    pub account: ProjectId,
    /// The password `bootstrap` generated, for a suite that logs in with it.
    /// `None` if the database already held an account.
    pub initial_password: Option<String>,
    /// The other end of this deployment's serving watch, present only when
    /// [`TestStateBuilder::stood_down`] built one. Held so the channel stays
    /// open for as long as the deployment does, and so a test that wants the
    /// node back can send `true`.
    pub serving: Option<tokio::sync::watch::Sender<bool>>,
}

impl TestState {
    /// The path a request for [`Self::account`] goes to.
    ///
    /// `path` is relative to the project, exactly as `http::scoped` writes it:
    /// `state.url("/sessions")`. A suite that hardcoded `/api/sessions` would
    /// still compile and would 404, which is why this exists.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("/api/p/{}{path}", self.account)
    }

    /// The bundle a request for [`Self::account`] resolves to.
    pub async fn services(&self) -> Arc<ProjectServices> {
        self.services_of(&self.account).await
    }

    /// The bundle a request for `project` resolves to, building it if this is
    /// the first time anything has asked.
    pub async fn services_of(&self, project: &ProjectId) -> Arc<ProjectServices> {
        self.state
            .projects
            .get(project)
            .await
            .expect("a project's services build")
    }

    /// A second project of [`Self::user`], for a suite asserting that two of
    /// one account's projects cannot see each other.
    pub async fn second_project(&self, name: &str) -> ProjectId {
        self.state
            .shared
            .project_service
            .create(&self.user, name)
            .await
            .expect("a second project is created")
            .id
    }

    /// Publish a connected vendor process under `name` for [`Self::account`].
    pub async fn publish_vendor(
        &self,
        name: &str,
        link: Arc<crate::runtime_vendor::WebsocketRuntimeVendor>,
    ) {
        self.services()
            .await
            .vendors
            .write()
            .unwrap()
            .insert(name.to_string(), link);
    }

    /// Register an LLM provider under `name` for [`Self::account`].
    ///
    /// No context window, so sessions on it never compact automatically —
    /// which is what almost every test wants. Use
    /// [`Self::insert_provider_with_window`] to exercise compaction.
    pub async fn insert_provider(
        &self,
        name: &str,
        provider: Arc<dyn horsie_agentcore::LlmProvider>,
    ) {
        self.insert_provider_with_window(name, provider, None).await;
    }

    /// Register an LLM provider whose card declares `context_window`.
    pub async fn insert_provider_with_window(
        &self,
        name: &str,
        provider: Arc<dyn horsie_agentcore::LlmProvider>,
        context_window: Option<u32>,
    ) {
        self.services()
            .await
            .provider_registry
            .write()
            .unwrap()
            .insert(
                name.to_string(),
                crate::sessions::spec::ModelEntry {
                    provider,
                    context_window,
                },
            );
    }

    /// Serve this state on an ephemeral port.
    ///
    /// No wait for the accept loop: the socket is listening from `bind`, so a
    /// connection made before `serve` first polls it waits in the backlog
    /// rather than being refused.
    pub async fn serve(&self) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::http::app(self.state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, task)
    }
}

/// Build a [`TestState`]. Start at [`state`].
pub struct TestStateBuilder {
    state_dir: PathBuf,
    db: Option<Db>,
    mode: AuthMode,
    supervisor: SupervisorConfig,
    /// What this deployment reports for [`crate::sessions::addressing::Serving`].
    /// `None` is the single-node default: never stands down, never gated.
    serving: Option<tokio::sync::watch::Sender<bool>>,
}

/// A Fly API root nothing is listening on, so a save's substrate check fails
/// as a refused connection — instantly, locally, and as "unreachable", which is
/// never a verdict on the configuration being saved.
///
/// The default for every test deployment: saving a Fly vendor calls this API,
/// and a suite that reached the real one would either fail on a bogus token or
/// spend a real request per save.
pub const UNREACHABLE_FLY_API: &str = "http://127.0.0.1:1/v1";

/// A deployment rooted at `state_dir`, on a fresh database, with auth off.
///
/// Auth off is the default because a disabled deployment is a real supported
/// configuration rather than a test-only escape, and it is what a suite driving
/// the API without a credential means. [`TestStateBuilder::auth`] turns it on.
pub fn state(state_dir: impl Into<PathBuf>) -> TestStateBuilder {
    TestStateBuilder {
        state_dir: state_dir.into(),
        db: None,
        mode: AuthMode::Off,
        supervisor: SupervisorConfig::default(),
        serving: None,
    }
}

impl TestStateBuilder {
    /// Use this database rather than a fresh one.
    ///
    /// What a restart test needs: a second incarnation is only a restart if it
    /// comes up on the journal the first one wrote.
    pub fn db(mut self, db: Db) -> Self {
        self.db = Some(db);
        self
    }

    pub fn auth(mut self, mode: AuthMode) -> Self {
        self.mode = mode;
        self
    }

    /// Put the idle policy under the test's control: a clock that only moves
    /// when told, and no background ticker, so an offload happens exactly when
    /// the test asks for one and never by surprise.
    pub fn supervisor(mut self, supervisor: SupervisorConfig) -> Self {
        self.supervisor = supervisor;
        self
    }

    /// A clustered node that has lost touch with a quorum.
    ///
    /// The one thing a single-node suite cannot otherwise produce, and the
    /// state every route has to answer for: a stood-down node holds actors it
    /// may no longer speak for, so its references refuse to send. What is being
    /// asserted through this is never behaviour of the cluster — it is what a
    /// handler does when its ask comes back undeliverable.
    pub fn stood_down(mut self) -> Self {
        self.serving = Some(tokio::sync::watch::channel(false).0);
        self
    }

    pub async fn build(self) -> TestState {
        let db = match self.db {
            Some(db) => db,
            None => crate::db::testing::db().await,
        };
        // One database for the whole deployment, as in production: auth's
        // tables live alongside everything else's, and giving auth a second
        // pool would hide any assertion that spans both.
        let auth = Arc::new(AuthService::new(
            AuthStore::new(db.clone()),
            AuthDeps {
                mode: self.mode,
                state_dir: self.state_dir.clone(),
            },
        ));
        let initial_password = auth.bootstrap().await.expect("bootstrap the first account");
        let user = auth
            .sole_user()
            .await
            .expect("read the bootstrapped account")
            .expect("bootstrap leaves exactly one account");

        let project_service = Arc::new(crate::projects::ProjectService::new(db.clone()));
        // Through the same call a first request makes, rather than by inserting
        // a row: a suite that got its default project a different way from
        // production would not notice production's way breaking.
        let account = project_service
            .default_project(&user)
            .await
            .expect("the bootstrap account gets a default project")
            .id;
        let shared = Arc::new(Shared {
            bus: Arc::new(crate::bus::MemoryBus::new()),
            system: crate::projects::node_system(&db, None),
            serving: self
                .serving
                .as_ref()
                .map(tokio::sync::watch::Sender::subscribe),
            db,
            project_service,
            artifacts: Arc::new(ArtifactStore::new(self.state_dir.join("plugins"))),
            info: info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: crate::config::model_cards::seed_marker(&[]),
            anonymous: user.clone(),
            supervisor: self.supervisor,
            deps: None,
            fly_api_base: UNREACHABLE_FLY_API.to_string(),
        });
        let projects = Arc::new(ProjectRegistry::new(shared.clone()));
        register_session_shards(&projects).expect("a fresh system has no shard types yet");
        let state = AppState {
            auth,
            projects,
            shared,
            web_dir: None,
        };
        TestState {
            state,
            user,
            account,
            serving: self.serving,
            initial_password,
        }
    }
}

/// The deployment paths a test reports. Empty: nothing here reads them, and a
/// plausible-looking path would invite something to start.
fn info() -> horsie_models::settings::ServerInfo {
    horsie_models::settings::ServerInfo {
        config_path: String::new(),
        database: String::new(),
        state_dir: String::new(),
        data_dir: String::new(),
        plugins_dir: String::new(),
        version: "test".into(),
    }
}

/// A throwaway deployment for a test that drives actors rather than routes.
///
/// Where [`TestState`] serves HTTP, this exists because a session or a
/// supervisor is built by a shard recipe now: it is handed an id and resolves
/// everything else from its account, so a test that wants one has to have an
/// account for it to resolve — and an account only exists inside a registry.
pub struct Deployment {
    pub projects: Arc<ProjectRegistry>,
    /// Every actor's log. In memory, so a test can read what was persisted and
    /// a restart is [`Self::restart`] rather than a second process.
    pub journal: Arc<dyn horsie_actor::Journal>,
    /// The one project everything here belongs to, and the account that owns
    /// it. `account` is the scope: it is what an actor address renders.
    pub account: ProjectId,
    pub user: UserId,
    _tmp: tempfile::TempDir,
}

impl Deployment {
    /// A deployment whose sessions run on `deps`, under `supervisor`'s policy.
    pub async fn new(
        deps: crate::sessions::spec::ServerDeps,
        supervisor: SupervisorConfig,
    ) -> Self {
        Self::on(
            Arc::new(horsie_actor::InMemoryJournal::new()),
            deps,
            supervisor,
        )
        .await
    }

    /// The same, over a journal the caller is watching.
    pub async fn on(
        journal: Arc<dyn horsie_actor::Journal>,
        deps: crate::sessions::spec::ServerDeps,
        supervisor: SupervisorConfig,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let project_service = Arc::new(crate::projects::ProjectService::new(db.clone()));
        let user = UserId::bootstrap();
        let account = project_service
            .default_project(&user)
            .await
            .expect("the bootstrap account gets a default project")
            .id;
        let projects = Arc::new(ProjectRegistry::new(Arc::new(Shared {
            bus: Arc::new(crate::bus::MemoryBus::new()),
            system: horsie_actor::ActorSystem::new(journal.clone()),
            serving: None,
            db,
            project_service,
            artifacts: Arc::new(ArtifactStore::new(tmp.path().join("artifacts"))),
            info: info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: crate::config::model_cards::seed_marker(&[]),
            anonymous: user.clone(),
            supervisor,
            deps: Some(deps),
            fly_api_base: UNREACHABLE_FLY_API.to_string(),
        })));
        crate::projects::register_session_shards(&projects)
            .expect("a fresh system has no shard types yet");
        Self {
            projects,
            journal,
            account,
            user,
            _tmp: tmp,
        }
    }

    pub async fn services(&self) -> Arc<ProjectServices> {
        self.projects
            .get(&self.account)
            .await
            .expect("an account's services build")
    }

    /// This account's supervisor, whether or not anything has loaded it.
    pub async fn supervisor(&self) -> crate::sessions::addressing::SupervisorRef {
        self.services().await.supervisor.clone()
    }

    /// One session of this account, whether or not it is loaded.
    pub fn session(&self, id: uuid::Uuid) -> crate::sessions::addressing::SessionRef {
        crate::sessions::addressing::SessionRef::new(
            self.projects
                .shared()
                .system
                .shard_actor_of::<crate::sessions::addressing::SessionShard>(),
            self.account.clone(),
            id,
            None,
        )
    }

    /// Every actor here, stopped — the process going away, as far as any of
    /// them is concerned. The next command addressed to one rebuilds it from
    /// its journal, which is the only way to reach recovery.
    pub async fn restart(&self) {
        self.projects
            .shared()
            .system
            .stop(&horsie_actor::ActorPath::root())
            .await;
    }
}

/// A journal that can stall one actor kind's writes on demand.
///
/// For asserting *when* something answers rather than what it answers with. A
/// promise that a reply means a durable write is only testable by stopping the
/// write and watching the reply not arrive; with an ordinary journal the write
/// wins the race by a mile and a broken promise passes.
///
/// Open until [`hold`](Self::hold) is called, so an actor can be started and
/// warmed first — a hold engaged before that stalls the actor's own recovery
/// and every later assertion is about the wrong thing.
pub struct HeldJournal {
    inner: Arc<dyn horsie_actor::Journal>,
    kind: String,
    lock: Arc<tokio::sync::RwLock<()>>,
}

impl HeldJournal {
    /// Wrap `inner`, ready to stall writes for `kind`. Other kinds always pass
    /// straight through, so only the log under test is ever stopped.
    pub fn wrapping(inner: Arc<dyn horsie_actor::Journal>, kind: &str) -> Arc<Self> {
        Arc::new(Self {
            inner,
            kind: kind.to_string(),
            lock: Arc::new(tokio::sync::RwLock::new(())),
        })
    }

    /// Stall `kind`'s writes until the returned guard is dropped.
    pub async fn hold(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.lock.clone().write_owned().await
    }
}

#[async_trait::async_trait]
impl horsie_actor::Journal for HeldJournal {
    async fn persist(
        &self,
        pid: &horsie_actor::PersistenceId,
        events: &[Vec<u8>],
        expected_last_seq: u64,
    ) -> horsie_actor::JournalResult<()> {
        let _open = if pid.kind == self.kind {
            Some(self.lock.read().await)
        } else {
            None
        };
        self.inner.persist(pid, events, expected_last_seq).await
    }

    // The rule is "read a journal through the actor that owns it". This is not a
    // read: it is the wrapper handing the call to the journal underneath, which
    // is the actor's own `replay` passing through.
    #[allow(clippy::disallowed_methods)]
    async fn replay(
        &self,
        pid: &horsie_actor::PersistenceId,
        after_seq: u64,
    ) -> futures_util::stream::BoxStream<'_, horsie_actor::JournalResult<(u64, Vec<u8>)>> {
        self.inner.replay(pid, after_seq).await
    }

    async fn save_snapshot(
        &self,
        pid: &horsie_actor::PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.save_snapshot(pid, state, seq_nr).await
    }

    async fn latest_snapshot(
        &self,
        pid: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<Option<(Vec<u8>, u64)>> {
        self.inner.latest_snapshot(pid).await
    }

    async fn delete_events_before(
        &self,
        pid: &horsie_actor::PersistenceId,
        seq_nr: u64,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.delete_events_before(pid, seq_nr).await
    }

    async fn copy_snapshot(
        &self,
        from: &horsie_actor::PersistenceId,
        to: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.copy_snapshot(from, to).await
    }

    async fn last_seq(
        &self,
        pid: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<u64> {
        self.inner.last_seq(pid).await
    }

    async fn clear(&self, pid: &horsie_actor::PersistenceId) -> horsie_actor::JournalResult<()> {
        self.inner.clear(pid).await
    }
}

/// Start an event-sourced actor at a throwaway path, reachable only by the
/// reference returned.
///
/// What `ActorSystem::spawn_persistent` used to do before horsie-actor 0.10
/// made creation go through a name. Production code creates children by name —
/// that is what gives them a path, a parent and a place in the tree — but a test
/// that wants one actor over one journal has no tree to put it in, and naming it
/// would only invite a second test to collide with it.
///
/// The `n`th call in a process gets its own path, so two actors in one test do
/// not land on top of each other.
pub fn spawn_detached<A: horsie_actor::EventSourcedActor>(
    system: &horsie_actor::ActorSystem,
    actor: A,
) -> horsie_actor::ActorRef<A::Command> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("detached-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    system.spawn_at(
        horsie_actor::ActorPath::root().child(&name),
        system.persistent(actor),
    )
}
