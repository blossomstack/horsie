//! The session server's HTTP surface: REST handlers + SSE streams over the
//! `SessionSupervisor`. All request/response bodies are fluorite wire types.

mod config;
pub mod error;
mod github;
mod handlers;
mod mcp;
mod plugins;
mod runtime_connect;
mod sse;

use crate::config::ConfigStore;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Router;
use axum::routing::{get, post, put};
use horsie_actor::{ActorRef, Journal};
use horsie_executor::{ConnectHook, ConnectedRuntimeRegistry};
use horsie_models::capabilities::CapabilitySpec;
use horsie_models::session::GlobalSessionEvent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};

/// Finalizes a request-supplied capability spec (path expansion, plugin grants,
/// platform seatbelt rules) — injected by the host binary, which owns the
/// capability-resolution helpers.
pub type CapsFinalize = Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync>;

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
    pub journal: Arc<dyn Journal>,
    pub global_events: broadcast::Sender<GlobalSessionEvent>,
    pub caps_finalize: CapsFinalize,
    /// Fully-resolved default capability spec for requests that omit one.
    pub default_caps: CapabilitySpec,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
    /// Reads and mutates the runtime-editable configuration (models, providers,
    /// default vendor). Also the source of the default vendor a create request
    /// falls back to when it omits one.
    pub config_store: Arc<dyn ConfigStore>,
    /// Deployment-global GitHub connection: App config, OAuth credentials, repo
    /// listing, and the scoped-token minter used at session provisioning.
    pub github: Arc<crate::github::GithubService>,
    /// Configured remote MCP servers: CRUD + connect/test, and the source of the
    /// per-session toolboxes built at agent spawn.
    pub mcp: Arc<crate::mcp::McpService>,
    /// DB-managed plugin-bundle library: install/list/update/delete and the
    /// token-guarded artifact endpoint runtimes fetch bundles from.
    pub plugins: Arc<crate::plugins::PluginService>,
    /// Server-wide registry every runtime dial-back (velos container or local
    /// daemon) lands in, keyed by `runtime_id`. Shared with the vendors so a
    /// vendor's `provision()` finds the connection the HTTP route registered.
    pub runtime_registry: Arc<ConnectedRuntimeRegistry>,
    /// Hook that registers a `?register=local` daemon as a vendor. `None` when
    /// the `local_runtime` opt-in is off, which makes the route refuse such
    /// registrations.
    pub local_daemon_hook: Option<ConnectHook>,
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
        .route("/api/sessions/:id/stop", post(handlers::stop_session))
        .route("/api/sessions/:id/events", get(sse::session_events))
        .route("/api/events", get(sse::global_events))
        .route(
            "/api/config",
            get(config::get_config).put(config::update_config),
        )
        .route("/api/config/vendors/:name/test", post(config::test_vendor))
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
            "/api/runtime/connect",
            get(runtime_connect::runtime_connect),
        )
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
    use crate::sessions::spec::ServerDeps;
    use crate::sessions::supervisor::SessionSupervisor;
    use crate::vendor::RuntimeVendor;
    use crate::vendor::mock::MockVendor;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use horsie_actor::{InMemoryJournal, spawn_root};
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    use horsie_models::session_api::{CreateSessionResponse, ListSessionsResponse};
    use std::collections::HashMap;
    use tower::util::ServiceExt;

    fn block_caps() -> CapabilitySpec {
        CapabilitySpec {
            network: NetworkPolicy::Block(BlockNetwork {}),
            grants: vec![],
            unsafe_seatbelt_rules: None,
        }
    }

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
        let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
        vendors.insert("mock".into(), Arc::new(MockVendor::new()));
        // A real DB store on a temp SQLite; the registry it opens is empty and
        // shared with the supervisor. `mock` is the runtime vendor under test.
        let db = tmp.path().join("config.db");
        let runtime_registry = Arc::new(ConnectedRuntimeRegistry::new());
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                info: test_info(),
                runtime_registry: runtime_registry.clone(),
            },
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
        let deps = ServerDeps {
            provider_registry: opened.registry,
            vendors: Arc::new(std::sync::RwLock::new(vendors)),
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: Some(mcp.clone()),
            plugins: None,
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = broadcast::channel(64);
        let supervisor = spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone());
        AppState {
            supervisor,
            journal,
            global_events: gtx,
            caps_finalize: Arc::new(|caps| caps),
            default_caps: block_caps(),
            plugins_dir: None,
            hook_path: vec![],
            config_store: opened.store,
            github,
            mcp,
            plugins,
            runtime_registry,
            local_daemon_hook: None,
            web_dir: None,
        }
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
        // message: mock vendor is fine but no provider for the model → 502
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/sessions/{id}/messages"),
                &serde_json::json!({"text": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
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
    async fn vendor_test_endpoint_round_trips() {
        use horsie_models::settings::VendorTestResult;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // A bound-then-dropped listener frees a port nothing listens on, so
        // the check fails fast (connection refused) instead of hanging.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);

        let body = serde_json::json!({
            "vendors": [{
                "name": "cluster-a",
                "config": {
                    "kind": "Velos",
                    "value": {
                        "serverUrl": format!("http://{dead_addr}"),
                        "image": "img",
                        "advertiseAddress": "10.0.0.5:3789",
                        "token": "tok"
                    }
                }
            }]
        });
        let res = app
            .clone()
            .oneshot(put_json("/api/config", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/config/vendors/cluster-a/test",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let result: VendorTestResult = read_json(res).await;
        assert!(!result.ok);
        assert!(result.error.is_some());

        // Unknown vendor name -> 500 (mirrors mcp::test's unknown-server case).
        let res = app
            .oneshot(post_json(
                "/api/config/vendors/ghost/test",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
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
        let refs = plugins.resolve(&["demo".into()], "http://x").await.unwrap();
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
}
