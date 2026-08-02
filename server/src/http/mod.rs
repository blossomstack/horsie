//! The session server's HTTP surface: REST handlers + SSE streams over the
//! `SessionSupervisor`. All request/response bodies are fluorite wire types.

mod admin;
mod auth;
mod config;
pub mod error;
mod github;
mod handlers;
mod mcp;
mod memory;
mod model_cards;
mod plugins;
mod sse;
pub mod vendor_connect;

use crate::auth::AuthService;
use crate::config::ConfigStore;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Router;
use axum::routing::{get, post, put};
use horsie_actor::ActorRef;
use horsie_models::session::GlobalSessionEvent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};

/// "http://host" from the request headers (horsie serves same-origin; a
/// configured `callback_base` overrides this inside a service). Shared by the
/// github and mcp OAuth callbacks.
pub(crate) fn request_base(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

#[derive(Clone)]
pub struct AppState {
    pub supervisor: ActorRef<SessionSupervisorCommand>,
    pub global_events: broadcast::Sender<GlobalSessionEvent>,
    /// Reads and mutates the runtime-editable configuration (models, providers,
    /// default vendor). Also the source of the default vendor a create request
    /// falls back to when it omits one.
    pub config_store: Arc<dyn ConfigStore>,
    /// The model-card catalog (reference data, not runtime config): public
    /// prefix search + admin CRUD. Shares the settings-DB pool.
    pub model_cards: Arc<crate::config::model_cards::ModelCardStore>,
    /// Deployment-global GitHub connection: App config, OAuth credentials, repo
    /// listing, and the scoped-token minter used at session provisioning.
    pub github: Arc<crate::github::GithubService>,
    /// Configured remote MCP servers: CRUD + connect/test, and the source of the
    /// per-session toolboxes built at agent spawn.
    pub mcp: Arc<crate::mcp::McpService>,
    /// DB-managed plugin-bundle library: install/list/update/delete and the
    /// token-guarded artifact endpoint runtimes fetch bundles from.
    pub plugins: Arc<crate::plugins::PluginService>,
    /// Agent-managed long-term memories: CRUD for the web UI. The agent reaches
    /// the same data through its `MemoryToolbox`, not over HTTP.
    pub memory: Arc<crate::memory::MemoryService>,
    /// Every connected vendor agent, published into the same vendor map
    /// sessions select from. Held here so the connect route can register a
    /// freshly handshaken link.
    pub vendor_agents: Arc<crate::runtime_vendor::RuntimeVendorRegistry>,
    /// The single admin account, the tokens it issues, and the policy the
    /// `/api` middleware applies. Disabled deployments get a service whose
    /// `enabled()` is false and which passes every request through.
    pub auth: Arc<AuthService>,
    /// Directory of built web-UI assets to serve alongside the API. When set,
    /// unmatched non-`/api` paths fall back to `index.html` (SPA routing), so
    /// the UI is served same-origin and no separate dev server is needed.
    pub web_dir: Option<PathBuf>,
}

pub fn app(state: AppState) -> Router {
    let web_dir = state.web_dir.clone();
    let api = Router::new()
        .route("/api/health", get(handlers::health))
        .route(
            "/api/sessions",
            post(handlers::create_session).get(handlers::list_sessions),
        )
        .route(
            "/api/sessions/:id",
            get(handlers::get_session).delete(handlers::delete_session),
        )
        .route("/api/sessions/:id/messages", post(handlers::send_message))
        .route("/api/sessions/:id/answers", post(handlers::answer_asks))
        .route("/api/sessions/:id/history", get(handlers::get_history))
        .route("/api/sessions/:id/subagents", get(handlers::get_subagents))
        .route("/api/sessions/:id/usage", get(handlers::get_session_usage))
        .route("/api/sessions/:id/stop", post(handlers::stop_session))
        .route("/api/sessions/:id/events", get(sse::session_events))
        .route("/api/events", get(sse::global_events))
        .route(
            "/api/config",
            get(config::get_config).put(config::update_config),
        )
        .route("/api/model-cards", get(model_cards::list))
        .route(
            "/api/admin/model-cards",
            get(admin::list_cards).post(admin::create_card),
        )
        .route(
            "/api/admin/model-cards/:model_id",
            put(admin::update_card).delete(admin::delete_card),
        )
        .route("/api/github/status", get(github::status))
        .route("/api/github/auth", get(github::auth))
        .route("/api/github/callback", get(github::callback))
        .route(
            "/api/github/app-config",
            get(github::get_app_config).put(github::put_app_config),
        )
        .route(
            "/api/github/disconnect",
            axum::routing::delete(github::disconnect),
        )
        .route("/api/github/repos", get(github::repos))
        .route("/api/github/repos/branches", get(github::branches))
        .route("/api/mcp/servers", get(mcp::list))
        .route(
            "/api/mcp/servers/:name",
            axum::routing::put(mcp::upsert).delete(mcp::delete),
        )
        .route("/api/mcp/servers/:name/test", post(mcp::test))
        .route("/api/mcp/servers/:name/connect", post(mcp::connect))
        .route(
            "/api/mcp/servers/:name/oauth/callback",
            get(mcp::oauth_callback),
        )
        .route("/api/plugins", get(plugins::list).post(plugins::install))
        .route(
            "/api/plugins/:name",
            put(plugins::set_default).delete(plugins::remove),
        )
        .route("/api/plugins/:name/update", post(plugins::update))
        .route("/api/plugin-artifacts/:file", get(plugins::get_artifact))
        .route(
            "/api/memory-spaces",
            get(memory::list_spaces).post(memory::create_space),
        )
        .route(
            "/api/memory-spaces/:name",
            put(memory::update_space).delete(memory::delete_space),
        )
        .route(
            "/api/memories",
            get(memory::list_memories).post(memory::create_memory),
        )
        .route(
            "/api/memories/:id",
            get(memory::get_memory)
                .put(memory::update_memory)
                .delete(memory::delete_memory),
        )
        .route("/api/vendor/connect", get(vendor_connect::vendor_connect))
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/password", post(auth::change_password))
        // Guards every route above. The SPA shell and its assets, added below,
        // are deliberately outside it: the app has to load in order to render a
        // login page, and the bundle holds no secrets.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state);

    match web_dir {
        // Serve the built UI: hashed assets and favicon from disk, and every
        // other (non-`/api`) path to index.html with a 200 so client-side
        // routes like `/sessions/:id` survive a hard refresh. Using `ServeFile`
        // as the fallback (rather than `not_found_service`) keeps the status 200.
        Some(dir) => api
            .nest_service("/assets", ServeDir::new(dir.join("assets")))
            .route_service("/favicon.svg", ServeFile::new(dir.join("favicon.svg")))
            .fallback_service(ServeFile::new(dir.join("index.html"))),
        None => api,
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
    use crate::runtime_vendor::RuntimeVendorLink;
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::spec::ServerDeps;
    use crate::sessions::supervisor::SessionSupervisor;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use horsie_models::session_api::{CreateSessionResponse, ListSessionsResponse};
    use std::collections::HashMap;
    use tower::util::ServiceExt;

    fn test_info() -> horsie_models::settings::ServerInfo {
        horsie_models::settings::ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        }
    }

    async fn test_state(tmp: &tempfile::TempDir) -> AppState {
        let mut vendors: HashMap<String, Arc<RuntimeVendorLink>> = HashMap::new();
        let mock_agent = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        vendors.insert("mock".into(), mock_agent.link());
        // A real DB store on a temp SQLite; the registry it opens is empty and
        // shared with the supervisor. `mock` is the runtime vendor under test.
        let db = tmp.path().join("config.db");
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps { info: test_info() },
        )
        .await
        .unwrap();
        let github = Arc::new(crate::github::GithubService::new(
            crate::github::GithubStore::new(opened.pool.clone()),
            crate::github::GithubApi::new(),
        ));
        let plugins = Arc::new(crate::plugins::PluginService::new(
            crate::plugins::PluginStore::new(opened.pool.clone()),
            crate::plugins::ArtifactStore::new(tmp.path().join("plugins")),
            b"test-secret".to_vec(),
        ));
        let mcp = Arc::new(crate::mcp::McpService::new(
            crate::mcp::McpStore::new(opened.pool.clone()),
            github.clone(),
        ));
        let memory = Arc::new(crate::memory::MemoryService::new(
            crate::memory::MemoryStore::new(opened.pool.clone()),
        ));
        let model_cards = Arc::new(crate::config::model_cards::ModelCardStore::new(
            opened.pool.clone(),
        ));
        let shared_vendors = Arc::new(std::sync::RwLock::new(vendors));
        let vendor_agents = Arc::new(crate::runtime_vendor::RuntimeVendorRegistry::new(
            shared_vendors.clone(),
        ));
        let deps = ServerDeps {
            provider_registry: opened.registry,
            runtimes: crate::runtime_manager::test_runtime_manager(&shared_vendors, tmp.path()),
            vendors: shared_vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: Some(mcp.clone()),
            plugins: None,
            memory: Some(memory.clone()),
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = broadcast::channel(64);
        let supervisor = spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone());
        // Auth off: every pre-existing test builds unauthenticated requests,
        // and a disabled deployment is a real supported configuration, not a
        // test-only escape. `auth_state` turns it on.
        let auth = Arc::new(crate::auth::AuthService::new(
            crate::auth::AuthStore::new(opened.pool.clone()),
            crate::auth::AuthDeps {
                enabled: false,
                state_dir: tmp.path().to_path_buf(),
            },
        ));
        AppState {
            supervisor,
            global_events: gtx,
            auth,
            config_store: opened.store,
            model_cards,
            github,
            mcp,
            plugins,
            memory,
            vendor_agents,
            web_dir: None,
        }
    }

    /// `test_state` with authentication enabled and the admin account
    /// bootstrapped. Returns the state and the generated password.
    ///
    /// Opens a second pool on the same file `test_state` already created and
    /// migrated, rather than reaching through the `Arc<dyn ConfigStore>` trait
    /// object for its pool — the auth tables live in that database, but auth
    /// has no business widening the config trait to get at them.
    async fn auth_state(tmp: &tempfile::TempDir) -> (AppState, String) {
        use std::str::FromStr;
        let mut state = test_state(tmp).await;
        let url = format!("sqlite://{}/config.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        let svc = Arc::new(crate::auth::AuthService::new(
            crate::auth::AuthStore::new(pool),
            crate::auth::AuthDeps {
                enabled: true,
                state_dir: tmp.path().to_path_buf(),
            },
        ));
        let password = svc.bootstrap().await.unwrap().expect("bootstrapped");
        state.auth = svc;
        (state, password)
    }

    /// The `Set-Cookie` session value from a login response.
    fn session_cookie(res: &axum::response::Response) -> String {
        let raw = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .unwrap();
        raw.split(';')
            .next()
            .unwrap()
            .trim_start_matches("horsie_session=")
            .to_string()
    }

    fn get_with_cookie(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::empty())
            .unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn delete(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    fn put_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    async fn read_json<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_responds_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app.oneshot(get("/api/health")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_list_get_message_lifecycle_over_http() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        // create
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "vendor": "mock"
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: CreateSessionResponse = read_json(res).await;
        let id = created.session.id;
        // list
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: ListSessionsResponse = read_json(res).await;
        assert_eq!(list.sessions.len(), 1);
        // get detail
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // unknown session → 404
        let res = app
            .clone()
            .oneshot(get("/api/sessions/00000000-0000-0000-0000-000000000000"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        // A message is always accepted: an unregistered model is a *turn*
        // failure the session reports later, not a rejection at the door.
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/sessions/{id}/messages"),
                &serde_json::json!({"text": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        // stop / delete
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/sessions/{id}/stop"),
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(delete(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // gone from the list
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let list: ListSessionsResponse = read_json(res).await;
        assert!(list.sessions.is_empty());
    }

    #[tokio::test]
    async fn config_get_and_put_round_trip() {
        use horsie_models::settings::SettingsView;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        // GET: fresh DB — no models, no configured vendors, and "local"
        // falls back to being the (unloaded) default since no daemon has
        // registered it and no other vendor is configured either.
        let res = app.clone().oneshot(get("/api/config")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.default_vendor, "local");
        assert!(view.models.is_empty());
        assert!(view.vendors.is_empty());
        // PUT a provider + model persists and redacts the key.
        let body = serde_json::json!({
            "providers": [{"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}],
            "models": [{"alias": "m", "provider": "p", "modelId": "id"}],
        });
        let res = app
            .clone()
            .oneshot(put_json("/api/config", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.models.len(), 1);
        assert!(view.providers[0].has_inline_key);
        // A model referencing a missing provider is a 422.
        let bad =
            serde_json::json!({ "models": [{"alias": "x", "provider": "ghost", "modelId": "y"}] });
        let res = app.oneshot(put_json("/api/config", &bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn create_without_repos_gets_managed_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "vendor": "mock"
        });
        let res = app
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn create_with_repos_builds_provision_steps() {
        use horsie_models::session_api::GetSessionResponse;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "vendor": "mock",
            "repos": [
                {"url": "https://github.com/o/api.git"},
                {"url": "https://github.com/o/web", "gitRef": "dev"}
            ]
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: CreateSessionResponse = read_json(res).await;
        let res = app
            .oneshot(get(&format!("/api/sessions/{}", created.session.id)))
            .await
            .unwrap();
        let detail: GetSessionResponse = read_json(res).await;
        assert_eq!(
            detail.session.repos,
            vec!["https://github.com/o/api.git", "https://github.com/o/web"]
        );
    }

    #[tokio::test]
    async fn github_status_and_app_config_round_trip() {
        use horsie_models::github::{GitHubAppConfigView, GitHubStatus};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // Fresh deployment: nothing configured.
        let res = app
            .clone()
            .oneshot(get("/api/github/status"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let s: GitHubStatus = read_json(res).await;
        assert!(!s.connected);
        assert!(!s.app_configured);

        // Save app config; secrets come back redacted.
        let body = serde_json::json!({
            "clientId": "cid", "clientSecret": "sec", "appId": 7, "privateKey": "PEM"
        });
        let res = app
            .clone()
            .oneshot(put_json("/api/github/app-config", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: GitHubAppConfigView = read_json(res).await;
        assert!(v.has_client_secret);
        assert!(v.has_private_key);
        assert_eq!(v.client_id, "cid");

        // Status now reports the app configured.
        let res = app
            .clone()
            .oneshot(get("/api/github/status"))
            .await
            .unwrap();
        let s: GitHubStatus = read_json(res).await;
        assert!(s.app_configured);

        // Auth redirect points at GitHub with our client id.
        let res = app.clone().oneshot(get("/api/github/auth")).await.unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(loc.contains("client_id=cid"), "{loc}");
    }

    #[tokio::test]
    async fn github_disconnect_without_credentials_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app.oneshot(delete("/api/github/disconnect")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn memory_spaces_and_memories_crud_over_http() {
        use horsie_models::memory::{MemorySpaceView, MemoryView};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // The migration seeds exactly one space.
        let res = app
            .clone()
            .oneshot(get("/api/memory-spaces"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let spaces: Vec<MemorySpaceView> = read_json(res).await;
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "default");
        assert_eq!(spaces[0].memory_count, 0);

        // Create a memory in it.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/memories",
                &serde_json::json!({
                    "space": "default",
                    "name": "alpha",
                    "description": "a durable fact",
                    "content": "the body"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: MemoryView = read_json(res).await;
        let id = created.id;
        assert_eq!(created.space, "default");

        // It shows up in the listing, and the space's count follows.
        let res = app
            .clone()
            .oneshot(get("/api/memories?space=default"))
            .await
            .unwrap();
        let listed: Vec<MemoryView> = read_json(res).await;
        assert_eq!(listed.len(), 1);
        let res = app
            .clone()
            .oneshot(get("/api/memory-spaces"))
            .await
            .unwrap();
        let spaces: Vec<MemorySpaceView> = read_json(res).await;
        assert_eq!(spaces[0].memory_count, 1);

        // Update only the content.
        let res = app
            .clone()
            .oneshot(put_json(
                &format!("/api/memories/{id}"),
                &serde_json::json!({ "content": "new body" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let updated: MemoryView = read_json(res).await;
        assert_eq!(updated.content, "new body");
        assert_eq!(updated.description, "a durable fact");

        // A bad slug is a 422, not a 500.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/memories",
                &serde_json::json!({
                    "space": "default", "name": "Bad Name",
                    "description": "d", "content": "c"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // A missing memory is a 404.
        let res = app
            .clone()
            .oneshot(get("/api/memories/99999"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Deleting the space takes its memories with it.
        let res = app
            .clone()
            .oneshot(delete("/api/memory-spaces/default"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app.oneshot(get("/api/memories")).await.unwrap();
        let all: Vec<MemoryView> = read_json(res).await;
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn plugins_install_list_artifact_delete_over_http() {
        use crate::plugins::PluginProvisioner;
        use horsie_models::plugins::PluginView;
        let tmp = tempfile::tempdir().unwrap();
        // A git plugin fixture (one skill).
        let repo = tmp.path().join("fixture");
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::write(
            repo.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("skills").join("a")).unwrap();
        std::fs::write(
            repo.join("skills").join("a").join("SKILL.md"),
            "---\nname: a\n---\nx",
        )
        .unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "i"]);
        let url = format!("file://{}", repo.display());

        let state = test_state(&tmp).await;
        let plugins = state.plugins.clone();
        let app = app(state);

        // Empty to start.
        let res = app.clone().oneshot(get("/api/plugins")).await.unwrap();
        let list: Vec<PluginView> = read_json(res).await;
        assert!(list.is_empty());

        // Install.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "sourceUrl": url }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let view: PluginView = read_json(res).await;
        assert_eq!(view.name, "demo");
        assert_eq!(view.skill_count, 1);

        // Listed.
        let res = app.clone().oneshot(get("/api/plugins")).await.unwrap();
        let list: Vec<PluginView> = read_json(res).await;
        assert_eq!(list.len(), 1);

        // Artifact fetch: 403 without a token, 200 with a valid bearer.
        let refs = plugins.resolve(&["demo".into()]).await.unwrap();
        let hash = refs[0].hash.clone();
        let res = app
            .clone()
            .oneshot(get(&format!("/api/plugin-artifacts/{hash}.zip")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let token = plugins.mint_token("s", std::slice::from_ref(&hash));
        let req = Request::builder()
            .uri(format!("/api/plugin-artifacts/{hash}.zip"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Delete.
        let res = app.oneshot(delete("/api/plugins/demo")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn mcp_server_crud_over_http() {
        use horsie_models::mcp::{McpAuthView, McpConnectResult, McpServerList, McpServerView};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // Upsert a bearer server; the token is redacted to `has_token` in the view.
        let body = serde_json::json!({
            "name": "ignored-by-path",
            "url": "http://127.0.0.1:0/",
            "auth": { "kind": "Bearer", "value": { "token": "sekret" } }
        });
        let res = app
            .clone()
            .oneshot(put_json("/api/mcp/servers/acme", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: McpServerView = read_json(res).await;
        assert_eq!(v.name, "acme"); // path is the id of record
        assert!(!v.enabled);
        match v.auth {
            McpAuthView::Bearer(b) => assert!(b.has_token),
            other => panic!("expected bearer auth, got {other:?}"),
        }

        // List reflects it.
        let res = app.clone().oneshot(get("/api/mcp/servers")).await.unwrap();
        let list: McpServerList = read_json(res).await;
        assert_eq!(list.servers.len(), 1);
        assert_eq!(list.servers[0].name, "acme");

        // Test against the unreachable URL: 200 with ok:false and an error.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/mcp/servers/acme/test",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let result: McpConnectResult = read_json(res).await;
        assert!(!result.ok);
        assert!(result.error.is_some());

        // Delete.
        let res = app
            .clone()
            .oneshot(delete("/api/mcp/servers/acme"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app.oneshot(get("/api/mcp/servers")).await.unwrap();
        let list: McpServerList = read_json(res).await;
        assert!(list.servers.is_empty());
    }

    #[tokio::test]
    async fn mcp_connect_on_non_oauth_is_unprocessable_and_callback_needs_code() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // A bearer server can't be OAuth-connected.
        let body = serde_json::json!({
            "name": "x", "url": "http://127.0.0.1:0/",
            "auth": { "kind": "Bearer", "value": { "token": "t" } }
        });
        app.clone()
            .oneshot(put_json("/api/mcp/servers/x", &body))
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/mcp/servers/x/connect",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // The callback without a code redirects to Settings with an error.
        let res = app
            .oneshot(get("/api/mcp/servers/x/oauth/callback"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(loc.starts_with("/settings?mcp_error="), "{loc}");
    }

    #[tokio::test]
    async fn model_cards_public_prefix_search() {
        use horsie_models::model_cards::{ModelCard, ModelCardInput};
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let store = state.model_cards.clone();
        let app = app(state);

        let input = |id: &str| ModelCardInput {
            model_id: id.into(),
            name: id.into(),
            context_window: Some(1000),
            max_tokens: None,
            thinking_efforts: None,
            default_thinking_effort: None,
            thinking_dialect: None,
            base_url: None,
            forced_tools_disable_thinking: None,
        };
        store
            .seed_if_missing(&[
                input("gpt-4o"),
                input("gpt-4.1"),
                input("claude-sonnet-4-6"),
            ])
            .await
            .unwrap();

        let res = app.clone().oneshot(get("/api/model-cards")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let all: Vec<ModelCard> = read_json(res).await;
        assert_eq!(all.len(), 3);

        let res = app
            .oneshot(get("/api/model-cards?prefix=gpt-4"))
            .await
            .unwrap();
        let hits: Vec<ModelCard> = read_json(res).await;
        assert_eq!(
            hits.iter().map(|c| c.model_id.as_str()).collect::<Vec<_>>(),
            ["gpt-4.1", "gpt-4o"]
        );
    }

    #[tokio::test]
    async fn admin_model_cards_crud_over_http() {
        use horsie_models::model_cards::ModelCard;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // Empty catalog (test_state does not seed).
        let res = app
            .clone()
            .oneshot(get("/api/admin/model-cards"))
            .await
            .unwrap();
        let list: Vec<ModelCard> = read_json(res).await;
        assert!(list.is_empty());

        // Create.
        let body = serde_json::json!({"modelId": "m1", "name": "Model One", "contextWindow": 8192});
        let res = app
            .clone()
            .oneshot(post_json("/api/admin/model-cards", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let card: ModelCard = read_json(res).await;
        assert_eq!(card.model_id, "m1");
        assert_eq!(card.max_tokens, None);

        // Duplicate → 409.
        let res = app
            .clone()
            .oneshot(post_json("/api/admin/model-cards", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Invalid → 422.
        let bad = serde_json::json!({"modelId": "", "name": "x"});
        let res = app
            .clone()
            .oneshot(post_json("/api/admin/model-cards", &bad))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Update.
        let upd = serde_json::json!({"name": "Model 1 Renamed", "maxTokens": 2048});
        let res = app
            .clone()
            .oneshot(put_json("/api/admin/model-cards/m1", &upd))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let card: ModelCard = read_json(res).await;
        assert_eq!(card.name, "Model 1 Renamed");
        assert_eq!(card.max_tokens, Some(2048));

        // Update of unknown → 404.
        let res = app
            .clone()
            .oneshot(put_json("/api/admin/model-cards/ghost", &upd))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Delete → 204; second delete → 404.
        let res = app
            .clone()
            .oneshot(delete("/api/admin/model-cards/m1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .oneshot(delete("/api/admin/model-cards/m1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_connected_agent_becomes_a_selectable_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let agents = state.vendor_agents.clone();
        let router = app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        assert!(
            !agents.connected_names().contains(&"my-laptop".to_string()),
            "no such vendor before the agent dials in"
        );

        let _agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("my-laptop")
            .supports_provisioning(false)
            .connect(&format!("ws://{addr}/api/vendor/connect"))
            .await
            .expect("agent connects");

        for _ in 0..100 {
            if agents.connected_names().contains(&"my-laptop".to_string()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("agent never registered as a vendor");
    }

    #[tokio::test]
    async fn with_auth_disabled_everything_is_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(get("/api/auth/status")).await.unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(!status.enabled);
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn with_auth_enabled_the_api_is_closed_but_health_and_status_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app.clone().oneshot(get("/api/health")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(get("/api/auth/status")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.enabled);
        assert!(!status.authenticated);
        // Never leaked to an anonymous caller.
        assert!(!status.must_change_password);
    }

    #[tokio::test]
    async fn login_sets_a_cookie_that_opens_the_api_and_logout_closes_it_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        // Wrong password.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": "nope"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Right password.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let raw_cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(raw_cookie.contains("HttpOnly"), "{raw_cookie}");
        assert!(raw_cookie.contains("SameSite=Lax"), "{raw_cookie}");
        assert!(raw_cookie.contains("Path=/"), "{raw_cookie}");
        let cookie = session_cookie(&res);

        // The cookie opens the API.
        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/sessions", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // ...and reports an authenticated status that admits the generated password.
        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/auth/status", &cookie))
            .await
            .unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.authenticated);
        assert!(status.must_change_password);

        // Logout revokes it.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header("cookie", format!("horsie_session={cookie}"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .oneshot(get_with_cookie("/api/sessions", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_bearer_token_is_accepted_and_a_bogus_one_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let secret = state.auth.login(&pw).await.unwrap();
        let app = app(state);

        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", "Bearer hsk_web_notarealtoken")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn changing_the_password_requires_the_current_one_and_then_works() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let change = |body: serde_json::Value, cookie: String| {
            Request::builder()
                .method("POST")
                .uri("/api/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("horsie_session={cookie}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": "wrong", "newPassword": "a-good-one"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": pw, "newPassword": "short"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": pw, "newPassword": "a-good-one"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The caller's own session survives, and the flag has cleared.
        let res = app
            .oneshot(get_with_cookie("/api/auth/status", &cookie))
            .await
            .unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.authenticated);
        assert!(!status.must_change_password);
    }

    #[tokio::test]
    async fn the_spa_shell_is_reachable_without_a_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let web = tmp.path().join("web");
        std::fs::create_dir_all(web.join("assets")).unwrap();
        std::fs::write(web.join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(web.join("favicon.svg"), "<svg/>").unwrap();

        let (mut state, _pw) = auth_state(&tmp).await;
        state.web_dir = Some(web);
        let app = app(state);

        let res = app.oneshot(get("/settings")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
