// Test scaffolding, not production code: a composition root that will not
// build is a broken test environment, and failing loudly where it fails beats
// threading a Result through every caller.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The real composition root, on a throwaway deployment.
//!
//! Every suite that drives horsie over HTTP needs the same thing: a `Shared`, a
//! `UserRegistry`, an `AuthService`, and an `AppState` wired together the way
//! `boot` wires them. Four suites used to assemble that by hand, which meant
//! four places to update when the root gained a field and four slightly
//! different deployments to reason about when one of them failed alone.
//!
//! Deliberately built *through* [`UserRegistry`] rather than by assembling a
//! bundle directly: what these tests exercise is what a request actually
//! resolves, including the lazy per-account build.

use crate::auth::{AuthDeps, AuthMode, AuthService, AuthStore, UserId};
use crate::db::Db;
use crate::http::AppState;
use crate::plugins::ArtifactStore;
use crate::sessions::supervisor::SupervisorConfig;
use crate::users::{Shared, UserRegistry, UserServices};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// A built deployment: the state a request resolves through, and the account it
/// resolves to when nothing else says otherwise.
pub struct TestState {
    pub state: AppState,
    /// The bootstrap account. Unauthenticated requests resolve here, and it is
    /// the one a single-account suite means by "the user".
    pub account: UserId,
    /// The password `bootstrap` generated, for a suite that logs in with it.
    /// `None` if the database already held an account.
    pub initial_password: Option<String>,
}

impl TestState {
    /// The bundle a request for [`Self::account`] resolves to.
    pub async fn services(&self) -> Arc<UserServices> {
        self.services_of(&self.account).await
    }

    /// The bundle a request for `account` resolves to, building it if this is
    /// the first time anything has asked.
    pub async fn services_of(&self, account: &UserId) -> Arc<UserServices> {
        self.state
            .users
            .get(account)
            .await
            .expect("an account's services build")
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
    pub async fn insert_provider(
        &self,
        name: &str,
        provider: Arc<dyn horsie_agentcore::LlmProvider>,
    ) {
        self.services()
            .await
            .provider_registry
            .write()
            .unwrap()
            .insert(name.to_string(), provider);
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
}

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
        let account = auth
            .sole_user()
            .await
            .expect("read the bootstrapped account")
            .expect("bootstrap leaves exactly one account");

        let shared = Arc::new(Shared {
            db,
            artifacts: Arc::new(ArtifactStore::new(self.state_dir.join("plugins"))),
            info: info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: crate::config::model_cards::seed_marker(&[]),
            anonymous: account.clone(),
            supervisor: self.supervisor,
        });
        let state = AppState {
            auth,
            users: Arc::new(UserRegistry::new(shared.clone())),
            shared,
            web_dir: None,
        };
        TestState {
            state,
            account,
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
