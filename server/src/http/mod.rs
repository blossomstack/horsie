//! The session server's HTTP surface: REST handlers + SSE streams over the
//! `SessionSupervisor`. All request/response bodies are fluorite wire types.

mod config;
pub mod error;
mod handlers;
mod sse;

use crate::config::ConfigStore;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Router;
use axum::routing::{get, post};
use horsie_actor::{ActorRef, Journal};
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
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                runtime_bin: std::path::PathBuf::from("horsie-runtime"),
                workspace_root: tmp.path().join("workspaces"),
                info: test_info(),
            },
        )
        .await
        .unwrap();
        let deps = ServerDeps {
            provider_registry: opened.registry,
            vendors,
            state_dir: tmp.path().to_path_buf(),
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
            "workdirs": ["/tmp"],
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
        // GET: fresh DB — no models, built-in `local` vendor is the default.
        let res = app.clone().oneshot(get("/api/config")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.default_vendor, "local");
        assert!(view.models.is_empty());
        assert!(view.vendors.iter().any(|v| v.name == "local"));
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
    async fn create_without_workdirs_gets_managed_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "workdirs": [],
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
            "workdirs": [],
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
        assert!(detail.session.workdirs.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_workdirs_and_repos_together() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "workdirs": ["/tmp"],
            "repos": [{"url": "https://github.com/o/x"}],
            "vendor": "mock"
        });
        let res = app
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
