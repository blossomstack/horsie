//! The session server's HTTP surface: REST handlers + SSE streams over the
//! `SessionSupervisor`. All request/response bodies are fluorite wire types.

mod admin;

mod annotations;
pub mod auth;
mod chatgpt;
mod config;
pub mod error;
pub(crate) mod github;
pub(crate) mod handlers;
mod marketplaces;
mod mcp;
mod memory;
pub(crate) mod messages;
mod model_cards;
mod plugins;
pub mod runtime_connect;
mod runtime_credentials;
mod runtime_pump;
mod runtime_vendors;
mod sse;
mod vendor_connect;
mod workflows;

use crate::auth::{AuthService, Principal};
use crate::users::{Shared, UserRegistry, UserServices};
use axum::Router;
use axum::routing::{get, post, put};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};

/// "scheme://host" from the request headers (horsie serves same-origin; a
/// configured `callback_base` overrides this inside a service). Shared by the
/// github and mcp OAuth callbacks.
///
/// The scheme has to come from `X-Forwarded-Proto`, because horsie itself
/// almost never terminates TLS — it sits behind Caddy, nginx or a cloud load
/// balancer, and sees plain HTTP on the inside no matter what the browser used.
/// Hardcoding `http://` here meant every OAuth `redirect_uri` went out as
/// `http://` on an HTTPS deployment, and GitHub rejects the mismatch outright,
/// so the flow could not complete anywhere it mattered. MCP OAuth builds its
/// redirect from the same function and had the same bug.
pub(crate) fn request_base(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if auth::arrived_over_tls(headers) {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

/// What the deployment owns, and how a request reaches what its account owns.
///
/// Everything scoped lives behind [`Scope`] instead. The split is what keeps
/// the routes that run *ahead* of the auth layer working: `/api/health`,
/// `/api/auth/*`, and `/api/plugin-artifacts/*` have no account to resolve.
#[derive(Clone)]
pub struct AppState {
    /// The accounts, the tokens they issue, and the policy the `/api`
    /// middleware applies. Disabled deployments get a service whose `enabled()`
    /// is false and which passes every request through.
    pub auth: Arc<AuthService>,
    /// The deployment tier: the pool, the artifact store, and what boot
    /// resolved once.
    pub shared: Arc<Shared>,
    /// Every account's services, built on first touch.
    pub users: Arc<UserRegistry>,
    /// Directory of built web-UI assets to serve alongside the API. When set,
    /// unmatched non-`/api` paths fall back to `index.html` (SPA routing), so
    /// the UI is served same-origin and no separate dev server is needed.
    pub web_dir: Option<PathBuf>,
}

/// The services belonging to whoever made this request.
///
/// Resolved per request from the [`Principal`] the auth middleware already put
/// in the extensions, because the credential is what carries the scope and one
/// process may hold several. A handler that takes this cannot reach another
/// account's anything: it holds that account's objects, not a filtered view of
/// everyone's.
pub struct Scope(pub Arc<UserServices>);

impl std::ops::Deref for Scope {
    type Target = UserServices;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl axum::extract::FromRequestParts<AppState> for Scope {
    type Rejection = error::Api;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Absent only if this route escaped `require_auth`, which would be a
        // routing bug rather than a caller's mistake — so it is a 500, and a
        // loud one, not a 401 that would read as "sign in again".
        let Some(principal) = parts.extensions.get::<Principal>() else {
            tracing::error!(
                path = %parts.uri.path(),
                "a scoped route ran without an authenticated principal"
            );
            return Err(error::Api::internal("could not resolve the account"));
        };
        let user = match principal {
            // Every request on a deployment with authentication disabled. The
            // account still exists — it is where the rows go.
            Principal::Anonymous => state.shared.anonymous.clone(),
            Principal::User(id) => id.clone(),
        };
        match state.users.get(&user).await {
            Ok(services) => Ok(Self(services)),
            Err(e) => {
                tracing::error!(user = %user, error = %e, "building an account's services failed");
                Err(error::Api::internal("could not resolve the account"))
            }
        }
    }
}

/// The credential surface, split by *whose* credential it is: `/api/auth/` is
/// the person in the browser, `/api/device/` is every credential a machine
/// holds — obtaining one, rotating it, approving one, and listing or revoking
/// the ones that exist.
///
/// Empty in [`AuthMode::Delegated`], where a layer in front owns identity and
/// serves both prefixes itself. Unmounted rather than answering 404: axum
/// panics when two merged routers claim one path, so leaving these out is
/// precisely what lets that layer claim them.
fn credentials(mode: crate::auth::AuthMode) -> Router<AppState> {
    if mode == crate::auth::AuthMode::Delegated {
        return Router::new();
    }
    Router::new()
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/password", post(auth::change_password))
        .route("/api/device/auth/code", post(auth::device_code))
        .route("/api/device/auth/token", post(auth::device_token))
        .route("/api/device/auth/refresh", post(auth::refresh))
        .route("/api/device/approve", post(auth::device_approve))
        .route("/api/device/deny", post(auth::device_deny))
        .route(
            "/api/device/tokens",
            get(auth::list_agent_tokens).post(auth::create_agent_token),
        )
        .route(
            "/api/device/tokens/{id}",
            axum::routing::delete(auth::delete_agent_token),
        )
}

pub fn app(state: AppState) -> Router {
    let web_dir = state.web_dir.clone();
    let api = Router::new()
        .route("/api/health", get(handlers::health))
        .route("/api/sessions", post(handlers::create_session))
        .route(
            "/api/sessions/{id}/annotations",
            put(annotations::set_annotations),
        )
        .route("/api/sessions/{id}/answers", post(handlers::answer_asks))
        .route(
            "/api/sessions/{id}/agents/{agent_id}",
            get(handlers::get_agent).delete(handlers::delete_fork),
        )
        .route(
            "/api/sessions/{id}/messages",
            post(handlers::send_message).get(messages::read_messages),
        )
        .route("/api/events", get(sse::global_events))
        .route("/api/config", get(config::get_config))
        .route(
            "/api/config/default-runtime-vendor",
            put(config::put_default_runtime_vendor).delete(config::delete_default_runtime_vendor),
        )
        .route("/api/config/models", get(config::list_models))
        .route(
            "/api/config/models/{alias}",
            put(config::put_model).delete(config::delete_model),
        )
        .route("/api/config/model-providers", get(config::list_providers))
        .route(
            "/api/config/model-providers/{name}",
            put(config::put_provider).delete(config::delete_provider),
        )
        .route(
            "/api/config/model-providers/{name}/chatgpt",
            get(chatgpt::status),
        )
        .route(
            "/api/config/model-providers/{name}/chatgpt/login",
            post(chatgpt::start).delete(chatgpt::sign_out),
        )
        .route(
            "/api/config/model-providers/{name}/chatgpt/poll",
            post(chatgpt::poll),
        )
        .route("/api/model-cards", get(model_cards::list))
        .route(
            "/api/admin/model-cards",
            get(admin::list_cards).post(admin::create_card),
        )
        .route(
            "/api/admin/model-cards/{model_id}",
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
            "/api/mcp/servers/{name}",
            axum::routing::put(mcp::upsert).delete(mcp::delete),
        )
        .route("/api/mcp/servers/{name}/test", post(mcp::test))
        .route("/api/mcp/servers/{name}/connect", post(mcp::connect))
        .route(
            "/api/mcp/servers/{name}/oauth/callback",
            get(mcp::oauth_callback),
        )
        .route("/api/marketplaces", get(marketplaces::list))
        .route(
            "/api/marketplaces/{name}",
            axum::routing::delete(marketplaces::remove),
        )
        .route(
            "/api/marketplaces/{name}/refresh",
            post(marketplaces::refresh),
        )
        .route("/api/plugins", get(plugins::list).post(plugins::install))
        .route("/api/builtins", get(plugins::builtins))
        .route(
            "/api/plugins/{name}",
            put(plugins::set_default).delete(plugins::remove),
        )
        .route("/api/plugins/{name}/update", post(plugins::update))
        .route("/api/plugin-artifacts/{file}", get(plugins::get_artifact))
        .route(
            "/api/memory-spaces",
            get(memory::list_spaces).post(memory::create_space),
        )
        .route(
            "/api/memory-spaces/{name}",
            put(memory::update_space).delete(memory::delete_space),
        )
        .route(
            "/api/memories",
            get(memory::list_memories).post(memory::create_memory),
        )
        .route(
            "/api/memories/{id}",
            get(memory::get_memory)
                .put(memory::update_memory)
                .delete(memory::delete_memory),
        )
        // Every JSON management route is mounted from the operation table
        // instead of listed here, so a new operation cannot exist without one.
        .merge(crate::control::http::router(&crate::control::operations()))
        .route(
            "/api/runtime-vendors",
            get(runtime_vendors::list_runtime_vendors),
        )
        .route(
            "/api/runtime-vendors/{name}",
            put(runtime_vendors::put_runtime_vendor).delete(runtime_vendors::delete_runtime_vendor),
        )
        .route(
            "/api/runtime-vendors/{name}/test",
            post(runtime_vendors::test_runtime_vendor),
        )
        .route("/api/sessions/{id}/workflow", get(workflows::get_run_graph))
        .route(
            "/api/sessions/{id}/workflow/retry",
            post(workflows::retry_step),
        )
        .route("/api/vendor/connect", get(vendor_connect::vendor_connect))
        .route(
            "/api/runtime/connect",
            get(runtime_connect::runtime_connect),
        )
        .route(
            "/api/runtime/github-credential",
            get(runtime_credentials::github_credential),
        )
        .merge(credentials(state.auth.mode()))
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
            // An unmatched `/api/*` path answers `404` in JSON rather than
            // falling through to the shell below. It used to serve
            // `200 text/html`, so a consumer checking status codes parsed an
            // HTML document as success — a typo in a path looked like an empty
            // result. A catch-all is matchit's lowest priority, so every real
            // route above still wins.
            .route(
                "/api/{*rest}",
                axum::routing::any(|| async {
                    axum::response::IntoResponse::into_response(error::Api::not_found(
                        "no such API route",
                    ))
                }),
            )
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
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use horsie_models::session_api::{CreateSessionResponse, ListSessionsResponse};
    use tower::util::ServiceExt;

    #[test]
    fn the_request_base_takes_its_scheme_from_the_forwarded_proto() {
        let base = |pairs: &[(&str, &str)]| {
            let mut h = axum::http::HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    axum::http::HeaderValue::from_str(v).unwrap(),
                );
            }
            request_base(&h)
        };

        // The bug: horsie sits behind a TLS terminator and sees plain HTTP, so
        // it built every OAuth `redirect_uri` as `http://` and GitHub rejected
        // the mismatch on every HTTPS deployment there is.
        assert_eq!(
            base(&[
                ("host", "horsie.example.com"),
                ("x-forwarded-proto", "https")
            ]),
            "https://horsie.example.com"
        );

        // A proxy chain forwards a list; the first entry is the client's.
        assert_eq!(
            base(&[
                ("host", "horsie.example.com"),
                ("x-forwarded-proto", "https, http"),
            ]),
            "https://horsie.example.com"
        );

        // No header is the plain-HTTP self-host shape, which must keep working.
        assert_eq!(base(&[("host", "localhost:3789")]), "http://localhost:3789");
        assert_eq!(
            base(&[("host", "localhost:3789"), ("x-forwarded-proto", "http")]),
            "http://localhost:3789"
        );
        // Anything unrecognised is not a promise of TLS.
        assert_eq!(
            base(&[("host", "localhost:3789"), ("x-forwarded-proto", "gopher")]),
            "http://localhost:3789"
        );
    }

    /// The real composition root, on a throwaway database, with one fake
    /// vendor process published under `mock` in the bootstrap account.
    async fn test_state(tmp: &tempfile::TempDir) -> AppState {
        let built = crate::testing::state(tmp.path()).build().await;
        publish_mock_vendor(&built.state).await;
        built.state
    }

    /// The dial token a runtime of the state's anonymous account would hold.
    ///
    /// Built from that account's real `runtime_dial_secret`, so it verifies the
    /// same way a live runtime's does — the point being that the artifact route
    /// now accepts exactly one thing, and it is the credential the runtime
    /// already has.
    async fn dial_token_for(state: &AppState, runtime_id: &str) -> String {
        let account = crate::auth::UserId::bootstrap();
        // Building the account is what creates its secret on first use.
        let _ = state.users.get(&account).await.unwrap();
        let secret = crate::config::dial_secret_of(&state.shared.db, &account)
            .await
            .unwrap()
            .expect("the account has a dial secret once it has been built");
        horsie_support::dial_token::mint(
            &secret,
            &horsie_support::dial_token::DialClaims {
                user_id: account.as_str().to_string(),
                runtime_id: runtime_id.to_string(),
                incarnation: "i1".to_string(),
            },
        )
    }

    /// Publish a fake vendor process as `mock` in the state's anonymous account,
    /// which is who every unauthenticated request resolves to.
    async fn publish_mock_vendor(state: &AppState) {
        let agent = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        services(state)
            .await
            .connected_vendors
            .publish(agent.link())
            .expect("mock is unclaimed in a fresh account");
    }

    /// The bundle an unauthenticated request resolves to.
    async fn services(state: &AppState) -> Arc<crate::users::UserServices> {
        state
            .users
            .get(&state.shared.anonymous)
            .await
            .expect("the anonymous account builds")
    }

    /// `test_state` with authentication enabled and the admin account
    /// bootstrapped. Returns the state and the generated password.
    ///
    /// One database for the whole deployment, as in production: the auth tables
    /// live alongside the config tables, and a second pool for auth alone would
    /// hide any assertion that spans both.
    async fn auth_state(tmp: &tempfile::TempDir) -> (AppState, String) {
        let built = crate::testing::state(tmp.path())
            .auth(crate::auth::AuthMode::Password)
            .build()
            .await;
        publish_mock_vendor(&built.state).await;
        let password = built.initial_password.expect("bootstrapped");
        (built.state, password)
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

    /// The contract every *created* named resource on this API keeps.
    ///
    /// `POST /api/thing` to create, `GET|PUT|DELETE /api/thing/{name}` to work
    /// on one. Four resources answer to exactly this, and used to say so in four
    /// hundred-line tests that each asserted ten behaviours in sequence — where
    /// the first failure hid the nine after it and reported only a status code.
    ///
    /// Not every resource is in here, and that is the point of a contract: the
    /// runtime-vendor and MCP-server routes are `PUT`-upsert with no `POST` and
    /// no item `GET`, so they keep no part of this and are tested on their own.
    /// What a resource does *differently* — which bodies it refuses, which
    /// fields a full replace clears, what it redacts — likewise stays its own
    /// test. This is the shape they share, not the reasons they differ.
    macro_rules! crud_over_http {
        (
            mod $module:ident,
            app: $app:path,
            path: $path:expr,
            name: $name:expr,
            create: $create:expr,
            replace: $replace:expr,
            replaced: $replaced:expr $(,)?
        ) => {
            mod $module {
                use super::*;

                fn item() -> String {
                    format!("{}/{}", $path, $name)
                }

                /// The app with the resource already created — the starting
                /// point for every test below except the create ones.
                async fn seeded(tmp: &tempfile::TempDir) -> axum::Router {
                    let app = $app(tmp).await;
                    let res = app
                        .clone()
                        .oneshot(post_json($path, &$create))
                        .await
                        .unwrap();
                    assert_eq!(
                        res.status(),
                        StatusCode::CREATED,
                        "the fixture's own create must succeed"
                    );
                    app
                }

                #[tokio::test]
                async fn creating_returns_201_and_the_stored_view() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = $app(&tmp).await;
                    let res = app.oneshot(post_json($path, &$create)).await.unwrap();
                    assert_eq!(res.status(), StatusCode::CREATED);
                    let v: serde_json::Value = read_json(res).await;
                    assert_eq!(v["name"], serde_json::json!($name));
                }

                #[tokio::test]
                async fn creating_the_same_name_twice_is_a_conflict() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.oneshot(post_json($path, &$create)).await.unwrap();
                    assert_eq!(res.status(), StatusCode::CONFLICT);
                }

                #[tokio::test]
                async fn the_list_holds_what_was_created() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.oneshot(get($path)).await.unwrap();
                    assert_eq!(res.status(), StatusCode::OK);
                    let list: Vec<serde_json::Value> = read_json(res).await;
                    assert_eq!(list.len(), 1);
                    assert_eq!(list[0]["name"], serde_json::json!($name));
                }

                #[tokio::test]
                async fn getting_it_by_name_returns_it() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.oneshot(get(&item())).await.unwrap();
                    assert_eq!(res.status(), StatusCode::OK);
                }

                #[tokio::test]
                async fn getting_a_name_that_does_not_exist_is_404() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.oneshot(get(&format!("{}/ghost", $path))).await.unwrap();
                    assert_eq!(res.status(), StatusCode::NOT_FOUND);
                }

                #[tokio::test]
                async fn replacing_it_returns_200_and_takes_effect() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.oneshot(put_json(&item(), &$replace)).await.unwrap();
                    assert_eq!(res.status(), StatusCode::OK);
                    let v: serde_json::Value = read_json(res).await;
                    #[allow(clippy::redundant_closure_call)]
                    ($replaced)(&v);
                }

                #[tokio::test]
                async fn replacing_a_name_that_does_not_exist_is_404() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let mut body = $replace;
                    body["name"] = serde_json::json!("ghost");
                    let res = app
                        .oneshot(put_json(&format!("{}/ghost", $path), &body))
                        .await
                        .unwrap();
                    assert_eq!(res.status(), StatusCode::NOT_FOUND);
                }

                /// A rename is not a replace: the name in the path is the
                /// identity, and a body that disagrees is refused rather than
                /// quietly moving the resource.
                #[tokio::test]
                async fn replacing_with_a_body_that_renames_is_422() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let mut body = $replace;
                    body["name"] = serde_json::json!("other");
                    let res = app.oneshot(put_json(&item(), &body)).await.unwrap();
                    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
                }

                #[tokio::test]
                async fn deleting_is_204_and_deleting_again_is_404() {
                    let tmp = tempfile::tempdir().unwrap();
                    let app = seeded(&tmp).await;
                    let res = app.clone().oneshot(delete(&item())).await.unwrap();
                    assert_eq!(res.status(), StatusCode::NO_CONTENT);
                    let res = app.oneshot(delete(&item())).await.unwrap();
                    assert_eq!(res.status(), StatusCode::NOT_FOUND);
                }
            }
        };
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
        // create — with the first message, which is the only shape there is
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
            "message": "first"
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
        // A fork answers with the conversation to open. camelCase on the wire —
        // a snake_case key here would read as absent and the assert would pass
        // for the wrong reason, so this reads the raw JSON.
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/sessions/{id}/messages"),
                &serde_json::json!({"text": "/fork try the other way"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let ack: serde_json::Value = read_json(res).await;
        let fork = ack["forkedAgent"]
            .as_str()
            .expect("a fork command answers with the agent to open")
            .to_string();
        assert!(
            uuid::Uuid::parse_str(&fork).is_ok(),
            "{fork} is an agent id"
        );

        // An ordinary message carries no fork, so a client never redirects.
        let res = app
            .clone()
            .oneshot(post_json(
                &format!("/api/sessions/{id}/messages"),
                &serde_json::json!({"text": "just talking"}),
            ))
            .await
            .unwrap();
        let ack: serde_json::Value = read_json(res).await;
        assert!(ack["forkedAgent"].is_null(), "{ack}");

        // The fork lists under its session, from the registry.
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let list: ListSessionsResponse = read_json(res).await;
        let row = list
            .sessions
            .iter()
            .find(|s| s.id == id)
            .expect("the session");
        assert!(
            row.forks.iter().any(|f| f.id == fork),
            "the fork is listed under its session: {:?}",
            row.forks
        );

        // And can be removed, but only because somebody asked.
        let res = app
            .clone()
            .oneshot(delete(&format!("/api/sessions/{id}/agents/{fork}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(delete(&format!("/api/sessions/{id}/agents/{fork}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "a fork goes once");

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
                &format!("/api/sessions/{id}/agents/main/stop"),
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

    /// The endpoints reject what they should before ever reaching OpenAI. The
    /// approved-login path is covered in `config::chatgpt_login`, against a fake
    /// issuer — there is nothing for an HTTP test to add to it.
    #[tokio::test]
    async fn chatgpt_login_rejects_unknown_and_non_chatgpt_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/config/model-providers/ghost/chatgpt/login",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/model-providers/p",
                &serde_json::json!({"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/config/model-providers/p/chatgpt/login",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // And a ChatGPT provider that nobody has signed into reports as much
        // rather than 404ing — the provider exists, the sign-in does not.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/model-providers/c",
                &serde_json::json!({"name": "c", "kind": "chatgpt"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .clone()
            .oneshot(get("/api/config/model-providers/c/chatgpt"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let status: serde_json::Value = read_json(res).await;
        assert_eq!(status["signedIn"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn config_get_and_per_resource_writes_round_trip() {
        use horsie_models::settings::SettingsView;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        // GET: fresh DB — no models, and "local" falls back to being the
        // (unloaded) default since nothing has set a preference. The one vendor
        // listed is the fake agent `test_state` published into this account's
        // map, which is the same map the settings view reads.
        let res = app.clone().oneshot(get("/api/config")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.default_runtime_vendor, "local");
        assert!(view.models.is_empty());
        assert_eq!(
            view.vendors.iter().map(|v| &v.name).collect::<Vec<_>>(),
            ["mock"]
        );
        // Writing a provider then a model persists both and redacts the key.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/model-providers/p",
                &serde_json::json!({"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/models/m",
                &serde_json::json!({"alias": "m", "provider": "p", "modelId": "id"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.clone().oneshot(get("/api/config")).await.unwrap();
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.models.len(), 1);
        assert!(view.providers[0].has_credential);

        // The default vendor has its own endpoint now, and clearing it is a
        // DELETE rather than an omitted field — which is what the old
        // whole-document save expressed, and why "clear" silently did nothing.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/default-runtime-vendor",
                &serde_json::json!({"vendor": "mock"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.default_runtime_vendor, "mock");

        let res = app
            .clone()
            .oneshot(delete_req("/api/config/default-runtime-vendor"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let view: SettingsView = read_json(res).await;
        assert_eq!(view.default_runtime_vendor, "local");

        // A model referencing a missing provider is a 422.
        let res = app
            .oneshot(put_json(
                "/api/config/models/x",
                &serde_json::json!({"alias": "x", "provider": "ghost", "modelId": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn create_without_repos_gets_managed_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
            "message": "hi"
        });
        let res = app
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    /// A session exists to be asked something. Creating one without a message
    /// used to provision a runtime that nothing would ever reclaim, so the
    /// field is required and an empty one is not a message.
    #[tokio::test]
    async fn a_session_cannot_be_created_without_a_first_message() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        for body in [
            serde_json::json!({
                "agent": {"model": "mock"},
                "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
            }),
            serde_json::json!({
                "agent": {"model": "mock"},
                "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
                "message": "  "
            }),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/sessions", &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        }
        // …and nothing was created on the way to refusing.
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let list: ListSessionsResponse = read_json(res).await;
        assert!(list.sessions.is_empty());
    }

    #[tokio::test]
    async fn create_with_repos_builds_provision_steps() {
        use horsie_models::session_api::GetSessionResponse;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "environment": {"type": "Runtime", "value": {"vendor": "mock", "repos": [
                {"url": "https://github.com/o/api.git"},
                {"url": "https://github.com/o/web", "gitRef": "dev"}
            ]}},
            "message": "hi"
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

    /// Create a session through the API and return its id.
    async fn create_session_via_api(app: &Router) -> String {
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/sessions",
                &serde_json::json!({
                    "agent": { "model": "mock" },
                    "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
                    "message": "hi"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = read_json(res).await;
        body["session"]["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn annotations_ride_the_session_list_and_survive_a_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state);
        let id = create_session_via_api(&app).await;

        // Tag it; the list carries the annotation.
        let res = app
            .clone()
            .oneshot(put_json(
                &format!("/api/sessions/{id}/annotations"),
                &serde_json::json!({ "set": [{ "key": "tag.web", "value": "" }], "remove": [] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let body: serde_json::Value = read_json(res).await;
        assert_eq!(
            body["sessions"][0]["annotations"],
            serde_json::json!([{ "key": "tag.web", "value": "" }])
        );

        // Removing the key is how a tag is unassigned — and, once no session
        // carries it, how the tag itself ceases to exist.
        let res = app
            .clone()
            .oneshot(put_json(
                &format!("/api/sessions/{id}/annotations"),
                &serde_json::json!({ "set": [], "remove": ["tag.web"] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        let body: serde_json::Value = read_json(res).await;
        assert_eq!(body["session"]["annotations"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn annotations_on_unknown_session_are_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state);
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/sessions/nope/annotations",
                &serde_json::json!({ "set": [], "remove": ["tag.web"] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
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

        // Save app config; secrets come back redacted. A real key, because the
        // save now parses it — see `validate_app_config`.
        let pem = include_str!("../github/testdata/test_rsa.pem");
        let body = serde_json::json!({
            "clientId": "cid", "clientSecret": "sec", "appId": 7, "privateKey": pem
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

    // The form performed no save-time validation at all: an empty save wiped a
    // working registration and reported a green SAVED, and a private key that
    // was not a key stored with `hasPrivateKey: true` and failed hours later at
    // the first clone.
    #[tokio::test]
    async fn app_config_refuses_what_it_cannot_use() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        for body in [
            serde_json::json!({ "clientId": "" }),
            serde_json::json!({ "clientId": "cid", "privateKey": "not-a-pem" }),
            // A value that is PEM-*shaped* but is not a key. This is the case
            // the old "does it start with -----BEGIN" check waved through, and
            // it stored with `hasPrivateKey: true`.
            serde_json::json!({
                "clientId": "cid",
                "privateKey": "-----BEGIN RSA PRIVATE KEY-----\nnope\n-----END RSA PRIVATE KEY-----\n",
            }),
            serde_json::json!({ "clientId": "cid", "callbackBase": "horsie.example.com" }),
        ] {
            let res = app
                .clone()
                .oneshot(put_json("/api/github/app-config", &body))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body} should be refused"
            );
        }

        // And nothing was stored on the way through.
        let res = app
            .clone()
            .oneshot(get("/api/github/status"))
            .await
            .unwrap();
        let s: horsie_models::github::GitHubStatus = read_json(res).await;
        assert!(!s.app_configured);
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

    /// The credential route is reachable by something holding only a dial
    /// token, and refuses everything it cannot justify.
    ///
    /// The status codes carry the point. `401` would mean `require_auth` ate
    /// the request before the route ever saw it — which is precisely what
    /// happened to `/api/runtime/connect` until it was allowlisted, and would
    /// mean no runtime on an authenticated deployment could ever get a
    /// credential. `403` means the route ran and said no.
    #[tokio::test]
    async fn a_github_credential_is_refused_without_a_verifiable_dial_token() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state.clone());

        let uri = "/api/runtime/github-credential?host=github.com&path=o/r";

        // No bearer at all.
        let res = app.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // A well-formed token signed with a secret that is not this account's.
        let forged = horsie_support::dial_token::mint(
            b"not-this-accounts-secret",
            &horsie_support::dial_token::DialClaims {
                user_id: crate::auth::UserId::bootstrap().as_str().to_string(),
                runtime_id: "rt-1".to_string(),
                incarnation: "i1".to_string(),
            },
        );
        let req = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {forged}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // This account's real token, but naming a session that does not exist.
        let token = dial_token_for(&state, "no-such-session").await;
        let req = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "reaching the route is the point; 401 would mean the auth layer \
             swallowed it before the route could check the token itself"
        );

        // A host this route does not serve, with an otherwise valid token.
        let req = Request::builder()
            .uri("/api/runtime/github-credential?host=gitlab.com&path=o/r")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    /// A session that never asked to clone a repository cannot mint a
    /// credential for it. The scope is the session's own `git_checkout` steps,
    /// not the account's whole GitHub installation.
    #[tokio::test]
    async fn a_repository_the_session_never_checks_out_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let session = {
            let services = services(&state).await;
            let mut spec = crate::sessions::spec::SessionSpec::for_vendor("mock");
            spec.provision
                .push(crate::sessions::spec::ProvisionStepSpec {
                    name: "checkout".into(),
                    uses: "git_checkout".into(),
                    with: vec![("url".into(), "https://github.com/o/wanted.git".into())],
                });
            services
                .supervisor
                .ask(
                    |reply| crate::sessions::supervisor::SessionSupervisorCommand::Create {
                        spec,
                        created_at: 0,
                        reply,
                    },
                )
                .await
                .unwrap()
        };
        let token = dial_token_for(&state, &session).await;
        let app = app(state.clone());

        let req = Request::builder()
            .uri("/api/runtime/github-credential?host=github.com&path=o/other")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn plugins_install_list_artifact_delete_over_http() {
        use crate::plugins::PluginProvisioner;
        use horsie_models::plugins::{InstallOutcome, PluginView};
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
            "---\nname: a\ndescription: d\n---\nx",
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
        let plugins = services(&state).await.plugins.clone();
        let app = app(state.clone());

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
        let InstallOutcome::Installed(view) = read_json::<InstallOutcome>(res).await else {
            panic!("a plain bundle repo installs rather than registering a source");
        };
        assert_eq!(view.name, "demo");
        assert_eq!(view.catalog.iter().filter(|e| e.kind == "skill").count(), 1);

        // Listed.
        let res = app.clone().oneshot(get("/api/plugins")).await.unwrap();
        let list: Vec<PluginView> = read_json(res).await;
        assert_eq!(list.len(), 1);

        // Artifact fetch: 403 without a bearer, 200 with this account's own
        // runtime's dial token.
        let refs = plugins.resolve(&["demo".into()]).await.unwrap();
        let hash = refs[0].hash.clone();
        let res = app
            .clone()
            .oneshot(get(&format!("/api/plugin-artifacts/{hash}.zip")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let token = dial_token_for(&state, "rt-1").await;
        let req = Request::builder()
            .uri(format!("/api/plugin-artifacts/{hash}.zip"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // A hash this account never installed is refused even with a valid
        // token — the boundary the old deployment-global secret did not have.
        let req = Request::builder()
            .uri("/api/plugin-artifacts/deadbeef.zip")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // A well-formed token signed with the wrong secret is refused, and
        // refused the same way — the route never confirms an account exists.
        let forged = horsie_support::dial_token::mint(
            b"not-this-accounts-secret",
            &horsie_support::dial_token::DialClaims {
                user_id: crate::auth::UserId::bootstrap().as_str().to_string(),
                runtime_id: "rt-1".to_string(),
                incarnation: "i1".to_string(),
            },
        );
        let req = Request::builder()
            .uri(format!("/api/plugin-artifacts/{hash}.zip"))
            .header("authorization", format!("Bearer {forged}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Delete.
        let res = app.oneshot(delete("/api/plugins/demo")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    /// A two-entry catalogue as a `file://` repo, plus the app under test.
    async fn app_with_catalogue(tmp: &tempfile::TempDir) -> (axum::Router, String) {
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::write(
            repo.join(".claude-plugin").join("marketplace.json"),
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        )
        .unwrap();
        for entry in ["alpha", "beta"] {
            let d = repo.join("plugins").join(entry).join("skills").join(entry);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {entry}\ndescription: d\n---\nx"),
            )
            .unwrap();
        }
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
        (app(test_state(tmp).await), url)
    }

    /// The one box: the same endpoint answers "installed it" and "here is a
    /// catalogue", so the client never has to classify a URL before sending it.
    #[tokio::test]
    async fn posting_a_catalogue_url_returns_a_marketplace_outcome() {
        use horsie_models::plugins::{InstallOutcome, MarketplaceView, PluginView};
        let tmp = tempfile::tempdir().unwrap();
        let (app, url) = app_with_catalogue(&tmp).await;

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "sourceUrl": url }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let InstallOutcome::Marketplace(view) = read_json::<InstallOutcome>(res).await else {
            panic!("a two-entry catalogue must not install anything");
        };
        assert_eq!(view.name, "catalogue");
        assert_eq!(view.plugin_count, 2);

        let res = app.clone().oneshot(get("/api/marketplaces")).await.unwrap();
        let list: Vec<MarketplaceView> = read_json(res).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].plugins.len(), 2);

        // Nothing was installed by registering the source.
        let res = app.oneshot(get("/api/plugins")).await.unwrap();
        let bundles: Vec<PluginView> = read_json(res).await;
        assert!(bundles.is_empty());
    }

    /// Neither input form is a 422 with a message, not a 500.
    #[tokio::test]
    async fn posting_an_empty_install_input_is_unprocessable() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _url) = app_with_catalogue(&tmp).await;
        let res = app
            .oneshot(post_json("/api/plugins", &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Removing a source is a 204 and leaves the bundle library alone.
    #[tokio::test]
    async fn deleting_a_marketplace_keeps_its_installed_bundles() {
        use horsie_models::plugins::PluginView;
        let tmp = tempfile::tempdir().unwrap();
        let (app, url) = app_with_catalogue(&tmp).await;

        app.clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "sourceUrl": url }),
            ))
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "marketplace": "catalogue", "pluginName": "alpha" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        let res = app
            .clone()
            .oneshot(delete("/api/marketplaces/catalogue"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app.oneshot(get("/api/plugins")).await.unwrap();
        let bundles: Vec<PluginView> = read_json(res).await;
        assert_eq!(bundles.len(), 1, "dropping a source keeps its software");
        assert_eq!(bundles[0].marketplace.as_deref(), Some("catalogue"));
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

    /// The two upsert-style resources do **not** agree on what deleting a name
    /// that is not there means, and nothing said so until this pair of tests.
    ///
    /// `runtime-vendors` reports the miss (`404`); `mcp/servers` returns `200`
    /// and deletes nothing, because its handler returns `Result<(), Api>` and
    /// its store never reports whether a row matched. Neither is wrong on its
    /// own — but a client cannot learn one from the other, so both are written
    /// down here. They are also why these two keep no shared CRUD contract:
    /// they have the shape in common and not the semantics.
    #[tokio::test]
    async fn deleting_an_unknown_runtime_vendor_reports_the_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app
            .oneshot(delete("/api/runtime-vendors/never-existed"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deleting_an_unknown_mcp_server_is_silently_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app
            .oneshot(delete("/api/mcp/servers/never-existed"))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "asserted as it behaves, not as it ought to: this is the asymmetry \
             with /api/runtime-vendors, and changing it is an API decision"
        );
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
        // The page that actually reads `mcp_error`. `/settings` redirects to
        // `/settings/models` and drops the query on the way.
        assert!(
            loc.starts_with("/settings/integrations?mcp_error="),
            "{loc}"
        );
    }

    #[tokio::test]
    async fn model_cards_public_prefix_search() {
        use horsie_models::model_cards::{ModelCard, ModelCardInput};
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let store = services(&state).await.model_cards.clone();
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

    /// Dial `/api/runtime/connect` with `token`, announce `announced`, and
    /// return the socket so the caller controls when it closes.
    async fn dial_runtime(
        addr: std::net::SocketAddr,
        token: &str,
        announced: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Error,
    > {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{addr}/api/runtime/connect")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
        let ready = serde_json::to_string(&horsie_models::runtime::RuntimeOutboundMessage::Ready(
            horsie_models::runtime::RuntimeReady {
                runtime_id: announced.to_string(),
            },
        ))
        .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(ready.into()))
            .await
            .unwrap();
        Ok(ws)
    }

    /// Bind and serve, returning the address. Named apart from the existing
    /// `serve` helper below, which answers with a URL string instead.
    async fn serve_state(state: AppState) -> std::net::SocketAddr {
        let router = app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    /// Subscribe to a runtime's out topic *before* anything dials, because the
    /// bus keeps nothing for a subscriber that has not arrived yet.
    async fn listening_to(
        state: &AppState,
        account: &str,
        runtime: &str,
    ) -> crate::bus::Reader<horsie_models::runtime::RuntimeOutboundMessage> {
        crate::bus::topics::runtime_out(state.shared.bus.clone(), account, runtime, "i1")
            .subscribe()
            .await
            .expect("subscribing to a runtime's out topic")
    }

    /// The runtime is reachable when its announcement reaches its topic — there
    /// is no registry to consult any more, and that is the point: the acquiring
    /// node need not be this one.
    #[tokio::test]
    async fn a_runtime_that_dials_in_is_reachable_on_its_own_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let services = services(&state).await;
        let account = services.user.as_str().to_string();
        let token = horsie_support::dial_token::mint(
            &services.dial_secret,
            &horsie_support::dial_token::DialClaims {
                user_id: account.clone(),
                runtime_id: "s1".to_string(),
                incarnation: "i1".to_string(),
            },
        );
        let mut announced = listening_to(&state, &account, "s1").await;
        let addr = serve_state(state).await;

        let _ws = dial_runtime(addr, &token, "s1").await.expect("dial");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), announced.recv())
            .await
            .expect("a dialled runtime must announce itself within the window")
            .expect("the topic must stay open");
        assert!(
            matches!(
                frame,
                horsie_models::runtime::RuntimeOutboundMessage::Ready(_)
            ),
            "expected a handshake on the runtime's out topic, got {frame:?}"
        );
    }

    #[tokio::test]
    async fn a_runtime_with_no_token_is_refused_before_the_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = serve_state(test_state(&tmp).await).await;
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/api/runtime/connect"))
                .await
                .is_err(),
            "an unauthenticated runtime must never reach a websocket"
        );
    }

    #[tokio::test]
    async fn a_token_cannot_register_a_runtime_it_does_not_name() {
        // The property the token buys. Before it, whoever announced an id first
        // received that runtime's relayed tool calls.
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let services = services(&state).await;
        let account = services.user.as_str().to_string();
        let token = horsie_support::dial_token::mint(
            &services.dial_secret,
            &horsie_support::dial_token::DialClaims {
                user_id: account.clone(),
                runtime_id: "mine".to_string(),
                incarnation: "i1".to_string(),
            },
        );
        let mut mine = listening_to(&state, &account, "mine").await;
        let mut theirs = listening_to(&state, &account, "someone-elses").await;
        let addr = serve_state(state).await;

        let _ws = dial_runtime(addr, &token, "someone-elses")
            .await
            .expect("dial");

        // The announcement lands on the topic the *token* names, which bounds the
        // negative assertion below on something real rather than on a sleep: by
        // the time `mine` has the frame, the pump has already routed it.
        let landed = tokio::time::timeout(std::time::Duration::from_secs(5), mine.recv())
            .await
            .expect("the token's own runtime must hear the announcement")
            .expect("the topic must stay open");
        assert!(matches!(
            landed,
            horsie_models::runtime::RuntimeOutboundMessage::Ready(_)
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), theirs.recv())
                .await
                .is_err(),
            "a token for 'mine' must not put anything on 'someone-elses' topic"
        );
    }

    /// The dial-back has to work on a deployment that has authentication on —
    /// which is every hosted one, and the only kind with cloud vendors to dial
    /// back from. A runtime holds no session credential, so `require_auth` used
    /// to answer 401 before this handler ever ran, and no Fly or velos machine
    /// could register at all. Both non-`Off` modes, because they refuse for
    /// different reasons: `Password` fails to verify the bearer, `Delegated`
    /// finds no identity attached.
    #[tokio::test]
    async fn a_runtime_dials_in_on_a_deployment_with_authentication_on() {
        for mode in [
            crate::auth::AuthMode::Password,
            crate::auth::AuthMode::Delegated,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = test_state(&tmp).await;
            state.auth = Arc::new(crate::auth::AuthService::new(
                crate::auth::AuthStore::new(crate::db::testing::db().await),
                crate::auth::AuthDeps {
                    mode,
                    state_dir: tmp.path().to_path_buf(),
                },
            ));
            let services = services(&state).await;
            let token = horsie_support::dial_token::mint(
                &services.dial_secret,
                &horsie_support::dial_token::DialClaims {
                    user_id: services.user.as_str().to_string(),
                    runtime_id: "s1".to_string(),
                    incarnation: "i1".to_string(),
                },
            );
            let mut announced = listening_to(&state, services.user.as_str(), "s1").await;
            let addr = serve_state(state).await;

            let _ws = dial_runtime(addr, &token, "s1")
                .await
                .unwrap_or_else(|e| panic!("a dial under {mode:?} must be accepted: {e}"));
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(5), announced.recv())
                    .await
                    .is_ok_and(|frame| frame.is_some()),
                "the runtime never reached its topic under {mode:?}"
            );
        }
    }

    /// Building an account is not free — a supervisor, a dial secret, a sweep
    /// task — and the account a token names is a *claim* until its tag checks
    /// out. Resolving the claim through the get-or-create registry meant a
    /// stranger could mint accounts by dialling in a loop with nonsense.
    #[tokio::test]
    async fn a_token_for_an_account_that_does_not_exist_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let users = state.users.clone();
        let addr = serve_state(state).await;

        assert!(
            dial_runtime(addr, "ghost.s1.deadbeef", "s1").await.is_err(),
            "a token no secret can verify must never reach a websocket"
        );
        assert!(
            !users.is_built(&crate::auth::UserId::new("ghost")),
            "an unverified claim must not have built an account"
        );
    }

    #[tokio::test]
    async fn a_connected_agent_becomes_a_selectable_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let agents = services(&state).await.connected_vendors.clone();
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

    /// A deployment whose identity comes from the layer in front of it.
    ///
    /// Built by hand rather than through `auth_state`: what makes this mode
    /// what it is, is that no credential of horsie's own exists.
    async fn delegated_state(tmp: &tempfile::TempDir) -> AppState {
        let built = crate::testing::state(tmp.path())
            .auth(crate::auth::AuthMode::Delegated)
            .build()
            .await;
        publish_mock_vendor(&built.state).await;
        built.state
    }

    /// The failure that would not announce itself.
    ///
    /// If an unidentified request fell back to the anonymous account instead of
    /// being refused, a deployment with one missing or mis-ordered middleware
    /// would serve every caller the *same* account's data while every request
    /// succeeded and every page rendered. So: `401`, and never `200`.
    #[tokio::test]
    async fn a_delegated_request_with_no_identity_is_refused_rather_than_anonymous() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(delegated_state(&tmp).await);

        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "an unidentified request must never resolve to the anonymous account"
        );

        // A bearer token is not an identity here: nothing in this deployment
        // issued one, so presenting anything must not change the answer.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .header("authorization", "Bearer anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // The liveness probe still answers: it resolves no account.
        let res = app.oneshot(get("/api/health")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// What the front layer's middleware does, and the whole point of the mode:
    /// the account it names is the account the request gets.
    #[tokio::test]
    async fn a_delegated_identity_resolves_to_that_account() {
        let tmp = tempfile::tempdir().unwrap();
        let state = delegated_state(&tmp).await;
        let anonymous = state.shared.anonymous.clone();
        let app = app(state);

        let mut req = get("/api/sessions");
        req.extensions_mut()
            .insert(crate::http::auth::DelegatedIdentity(anonymous));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Unmounted, not stubbed — and the reason it has to be unmounted rather
    /// than answering 404 is that axum *panics* when two merged routers claim
    /// one path. So the property worth asserting is not horsie's status code:
    /// it is that a layer in front can take those paths over and be the one
    /// that answers.
    #[tokio::test]
    async fn a_front_layer_can_claim_every_credential_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = [
            "/api/auth/status",
            "/api/auth/login",
            "/api/auth/logout",
            "/api/auth/password",
            "/api/auth/session",
            "/api/device/auth/code",
            "/api/device/auth/token",
            "/api/device/auth/refresh",
            "/api/device/approve",
            "/api/device/deny",
            "/api/device/tokens",
        ];

        // Merging these onto a delegated deployment must not panic, and each
        // one must reach the front layer's handler rather than horsie's.
        let mut front = Router::new();
        for path in paths {
            front = front.route(path, post(|| async { "front layer" }));
        }
        let app = app(delegated_state(&tmp).await).merge(front);

        for path in paths {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let body = axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .unwrap();
            assert_eq!(
                &body[..],
                b"front layer",
                "{path} must be answered by the layer in front, not by horsie"
            );
        }
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

    #[tokio::test]
    async fn an_unknown_api_path_is_a_json_404_not_the_spa_shell() {
        // The SPA fallback used to swallow `/api/*` too, so a mistyped path
        // answered `200 text/html` and a consumer checking status codes parsed
        // an HTML document as success.
        let tmp = tempfile::tempdir().unwrap();
        let web = tmp.path().join("web");
        std::fs::create_dir_all(web.join("assets")).unwrap();
        std::fs::write(web.join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(web.join("favicon.svg"), "<svg/>").unwrap();

        let (mut state, _pw) = auth_state(&tmp).await;
        state.web_dir = Some(web);
        let app = app(state);

        let res = app.clone().oneshot(get("/api/nope")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );

        // A real route still wins over the catch-all: matchit ranks a literal
        // segment above a wildcard, but that is worth pinning rather than
        // trusting.
        let res = app.clone().oneshot(get("/api/health")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // And a client-side route still gets the shell, or a hard refresh on
        // `/sessions/:id` would 404.
        let res = app.oneshot(get("/sessions/abc")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Seed a usable server: one provider, one model, and `mock` as the default
    /// vendor. The default vendor matters because a preset names none — every
    /// invocation resolves it.
    ///
    /// Three requests rather than one, because each resource is addressed on
    /// its own now. Provider first: the model referencing it is validated
    /// against what is stored.
    async fn put_mock_model(app: &axum::Router) {
        for (uri, body) in [
            (
                "/api/config/model-providers/p",
                serde_json::json!({"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}),
            ),
            (
                "/api/config/models/mock",
                serde_json::json!({"alias": "mock", "provider": "p", "modelId": "id"}),
            ),
            (
                "/api/config/default-runtime-vendor",
                serde_json::json!({"vendor": "mock"}),
            ),
        ] {
            let res = app.clone().oneshot(put_json(uri, &body)).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "seeding {uri}");
        }
    }

    #[tokio::test]
    async fn an_agent_needs_a_slug_name_and_a_model_that_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let app = agent_app(&tmp).await;
        for bad in [
            serde_json::json!({"name": "Bad Name", "model": "mock"}),
            serde_json::json!({"name": "x", "model": "ghost"}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/agents", &bad))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
    }

    /// A `PUT` that omits a list clears it, rather than merging into what was
    /// there — the difference between a replace and a patch, on the field where
    /// a silent merge would keep granting a plugin the operator removed.
    #[tokio::test]
    async fn replacing_an_agent_clears_the_plugins_it_omits() {
        use horsie_models::agents::AgentView;
        let tmp = tempfile::tempdir().unwrap();
        let app = agent_app(&tmp).await;
        let created = serde_json::json!({
            "name": "reviewer", "description": "reviews PRs", "model": "mock",
            "plugins": ["superpowers"], "memorySpaces": ["default"]
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/agents", &created))
            .await
            .unwrap();
        let v: AgentView = read_json(res).await;
        assert_eq!(v.plugins, vec!["superpowers".to_string()]);

        let res = app
            .oneshot(put_json(
                "/api/agents/reviewer",
                &serde_json::json!({"name": "reviewer", "model": "mock", "description": "v2"}),
            ))
            .await
            .unwrap();
        let v: AgentView = read_json(res).await;
        assert!(v.plugins.is_empty(), "PUT is a full replace");
    }

    /// An app with a "mock" model and a "reviewer" agent preset on the
    /// connected "mock" vendor — everything a routine needs to be runnable.
    /// An app with a "mock" model, which is all an agent preset needs.
    async fn agent_app(tmp: &tempfile::TempDir) -> axum::Router {
        let app = app(test_state(tmp).await);
        put_mock_model(&app).await;
        app
    }

    /// An app with nothing extra — what an environment needs.
    async fn plain_app(tmp: &tempfile::TempDir) -> axum::Router {
        app(test_state(tmp).await)
    }

    async fn routine_app(tmp: &tempfile::TempDir) -> axum::Router {
        let app = app(test_state(tmp).await);
        put_mock_model(&app).await;
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents",
                &serde_json::json!({"name": "reviewer", "model": "mock"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        app
    }

    fn workflow_body() -> serde_json::Value {
        serde_json::json!({
            "name": "fix-bug",
            "description": "triage then fix",
            "start": "triage",
            "steps": [
                {
                    "name": "triage",
                    "agent": "reviewer",
                    "prompt": "Triage it.",
                    "outcomes": [
                        {"value": "p0", "description": "drop everything"},
                        {"value": "p2", "description": "file it"}
                    ],
                    "transitions": [
                        {"to": "fix", "when": {"op": "In", "value": {"values": ["p0"]}}},
                        {"to": "fix"}
                    ]
                },
                {"name": "fix", "agent": "reviewer", "prompt": "Fix it."}
            ]
        })
    }

    /// Every graph defect is refused, rather than a workflow saved broken.
    #[tokio::test]
    async fn a_workflow_whose_graph_does_not_hold_together_is_422() {
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        for bad in [
            // start names no step
            serde_json::json!({"name": "b", "start": "nowhere",
                               "steps": [{"name": "a", "agent": "reviewer", "prompt": "x"}]}),
            // transition to a step that does not exist
            serde_json::json!({"name": "b", "start": "a",
                               "steps": [{"name": "a", "agent": "reviewer", "prompt": "x",
                                          "transitions": [{"to": "ghost"}]}]}),
            // an outcome value with no description
            serde_json::json!({"name": "b", "start": "a",
                               "steps": [{"name": "a", "agent": "reviewer", "prompt": "x",
                                          "outcomes": [{"value": "ok", "description": ""}]}]}),
            // unknown preset
            serde_json::json!({"name": "b", "start": "a",
                               "steps": [{"name": "a", "agent": "ghost", "prompt": "x"}]}),
            // duplicate step names
            serde_json::json!({"name": "b", "start": "a",
                               "steps": [{"name": "a", "agent": "reviewer", "prompt": "x"},
                                         {"name": "a", "agent": "reviewer", "prompt": "y"}]}),
            // no steps at all
            serde_json::json!({"name": "b", "start": "a", "steps": []}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/workflows", &bad))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "expected 422 for {bad}"
            );
        }
    }

    /// Transition order decides which condition wins, so it has to survive the
    /// wire — an unconditional fallback reordered ahead of a guard would take
    /// every branch.
    #[tokio::test]
    async fn a_workflows_steps_and_transition_order_survive_the_wire() {
        use horsie_models::workflow::WorkflowView;
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        let res = app
            .oneshot(post_json("/api/workflows", &workflow_body()))
            .await
            .unwrap();
        let v: WorkflowView = read_json(res).await;
        assert_eq!(v.start, "triage");
        assert_eq!(v.steps.len(), 2);
        let t = v.steps[0].transitions.as_ref().unwrap();
        assert_eq!(t.len(), 2);
        assert!(t[1].when.is_none());
    }

    crud_over_http! {
        mod agents_contract,
        app: agent_app,
        path: "/api/agents",
        name: "reviewer",
        create: serde_json::json!({
            "name": "reviewer", "description": "reviews PRs", "model": "mock",
            "plugins": ["superpowers"], "memorySpaces": ["default"]
        }),
        replace: serde_json::json!({"name": "reviewer", "model": "mock", "description": "v2"}),
        replaced: |v: &serde_json::Value| assert_eq!(v["description"], "v2"),
    }

    crud_over_http! {
        mod environments_contract,
        app: plain_app,
        path: "/api/environments",
        name: "staging",
        create: serde_json::json!({
            "name": "staging", "description": "fly box", "vendor": "fly",
            "repos": [{"url": "https://github.com/o/api", "gitRef": "dev"}],
            "envVars": [{"name": "RUST_LOG", "value": "debug"}],
            "provision": [{"name": "setup", "uses": "run", "with": [{"key": "cmd", "value": "make setup"}]}],
        }),
        replace: serde_json::json!({"name": "staging", "vendor": "docker"}),
        replaced: |v: &serde_json::Value| assert_eq!(v["vendor"], "docker"),
    }

    crud_over_http! {
        mod routines_contract,
        app: routine_app,
        path: "/api/routines",
        name: "nightly",
        create: routine_body(),
        replace: {
            let mut b = routine_body();
            b["description"] = serde_json::json!("v2");
            b
        },
        replaced: |v: &serde_json::Value| assert_eq!(v["description"], "v2"),
    }

    crud_over_http! {
        mod workflows_contract,
        app: routine_app,
        path: "/api/workflows",
        name: "fix-bug",
        create: workflow_body(),
        replace: {
            let mut b = workflow_body();
            b["description"] = serde_json::json!("v2");
            b
        },
        replaced: |v: &serde_json::Value| assert_eq!(v["description"], "v2"),
    }

    fn routine_body() -> serde_json::Value {
        serde_json::json!({
            "name": "nightly", "description": "triage", "agent": "reviewer",
            "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
            "prompt": "triage the inbox",
            "schedule": {"type": "Every", "value": {"intervalSecs": 3600}}
        })
    }

    #[tokio::test]
    async fn a_routine_needs_a_slug_name_a_real_agent_a_prompt_and_a_sane_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        for bad in [
            serde_json::json!({"name": "Bad Name", "agent": "reviewer", "prompt": "x"}),
            serde_json::json!({"name": "b", "agent": "ghost", "prompt": "x"}),
            serde_json::json!({"name": "b", "agent": "reviewer", "prompt": "  "}),
            serde_json::json!({"name": "b", "agent": "reviewer", "prompt": "x",
                               "schedule": {"type": "Every", "value": {"intervalSecs": 5}}}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/routines", &bad))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
    }

    /// Creating arms the schedule; a replace that omits it disarms, because a
    /// `PUT` is a full replace and `Manual` is what "no schedule" means.
    #[tokio::test]
    async fn a_routines_schedule_is_armed_on_create_and_replaced_wholesale() {
        use horsie_models::routines::RoutineView;
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        let res = app
            .clone()
            .oneshot(post_json("/api/routines", &routine_body()))
            .await
            .unwrap();
        let v: RoutineView = read_json(res).await;
        assert_eq!(v.agent, "reviewer");
        assert!(v.enabled);
        assert!(v.next_run_at_ms.is_some());

        let res = app
            .oneshot(put_json(
                "/api/routines/nightly",
                &serde_json::json!({
                    "name": "nightly", "agent": "reviewer", "prompt": "new prompt", "enabled": false,
                    "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
                }),
            ))
            .await
            .unwrap();
        let v: RoutineView = read_json(res).await;
        assert_eq!(v.prompt, "new prompt");
        assert!(!v.enabled);
        assert_eq!(v.next_run_at_ms, None, "a paused routine is not armed");
        assert_eq!(
            v.schedule,
            horsie_models::routines::RoutineSchedule::Manual(
                horsie_models::routines::ManualSchedule {}
            ),
            "PUT is a full replace"
        );
    }

    #[tokio::test]
    async fn runtime_vendors_crud_over_http() {
        use horsie_models::runtime_vendor::{RuntimeVendorConfigView, RuntimeVendorSettings};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        let settings = |callback: &str| {
            // Unions are adjacently tagged across this protocol: a variant name
            // in `kind`, its payload in `value`.
            serde_json::json!({
                "kind": "Fly",
                "value": {
                    "app": "horsie-runtimes",
                    "image": "ghcr.io/o/runtime:1",
                    "region": "iad",
                    "workspaceRoot": "/workspaces",
                    "callbackUrl": callback,
                    "volumes": true,
                    "cpuKind": "shared",
                    "cpus": 1,
                    "memoryMb": 1024,
                    "volumeSizeGb": 10,
                }
            })
        };
        let body = serde_json::json!({
            "name": "fly",
            "settings": settings("wss://horsie.example.com/api/runtime/connect"),
            "credential": "fly-token",
        });

        let res = app
            .clone()
            .oneshot(put_json("/api/runtime-vendors/fly", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: RuntimeVendorConfigView = read_json(res).await;
        assert!(v.has_credential, "the token was stored");
        let RuntimeVendorSettings::Fly(fly) = v.settings else {
            panic!("a fly vendor must round-trip as one")
        };
        assert_eq!(
            fly.callback_url, "wss://horsie.example.com/api/runtime/connect",
            "the callback url is stored exactly as it was sent"
        );

        // The token is never readable back, however it is asked for.
        let res = app
            .clone()
            .oneshot(get("/api/runtime-vendors"))
            .await
            .unwrap();
        let list: Vec<RuntimeVendorConfigView> = read_json(res).await;
        assert_eq!(list.len(), 1);
        assert!(list[0].has_credential);
        assert!(
            !serde_json::to_string(&list[0])
                .unwrap()
                .contains("fly-token"),
            "a stored credential must never be serialised back to a client"
        );

        // An edit may omit the credential, since the client cannot read it.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/runtime-vendors/fly",
                &serde_json::json!({
                    "name": "fly",
                    "settings": settings("wss://horsie.example.com/relay"),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: RuntimeVendorConfigView = read_json(res).await;
        assert!(
            v.has_credential,
            "an omitted credential keeps the stored one"
        );

        // A callback only the server itself can reach is refused at save time.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/runtime-vendors/local-only",
                &serde_json::json!({
                    "name": "local-only",
                    "settings": settings("ws://localhost:8080/api/runtime/connect"),
                    "credential": "t",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // The substrate is unreachable from a test deployment, so a check
        // answers rather than errors — the failure being tested for is the
        // route reporting one as a 5xx, or as a saved vendor's absence.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/runtime-vendors/fly/test",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let result: horsie_models::runtime_vendor::RuntimeVendorTestResult = read_json(res).await;
        assert!(!result.ok);
        assert!(result.error.is_some(), "a failure has to say what happened");

        // A name nothing is configured under is a different thing, and says so.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/runtime-vendors/nobody/test",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = app
            .clone()
            .oneshot(delete("/api/runtime-vendors/fly"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .oneshot(delete("/api/runtime-vendors/fly"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_environment_needs_a_slug_name_and_a_real_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let app = plain_app(&tmp).await;
        for bad in [
            serde_json::json!({"name": "Bad Name", "vendor": "fly"}),
            serde_json::json!({"name": "b", "vendor": ""}),
            serde_json::json!({"name": "b", "vendor": "local"}),
        ] {
            let res = app
                .clone()
                .oneshot(post_json("/api/environments", &bad))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
    }

    /// The nested collections survive the wire on create, and a `PUT` that
    /// omits them clears them.
    #[tokio::test]
    async fn an_environments_repos_env_vars_and_provision_round_trip() {
        use horsie_models::environments::EnvironmentView;
        let tmp = tempfile::tempdir().unwrap();
        let app = plain_app(&tmp).await;
        let body = serde_json::json!({
            "name": "staging", "description": "fly box", "vendor": "fly",
            "repos": [{"url": "https://github.com/o/api", "gitRef": "dev"}],
            "envVars": [{"name": "RUST_LOG", "value": "debug"}],
            "provision": [{"name": "setup", "uses": "run", "with": [{"key": "cmd", "value": "make setup"}]}],
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/environments", &body))
            .await
            .unwrap();
        let v: EnvironmentView = read_json(res).await;
        assert_eq!(v.vendor, "fly");
        assert_eq!(v.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(v.env_vars[0].name, "RUST_LOG");
        assert_eq!(v.provision[0].uses, "run");

        let res = app
            .oneshot(put_json(
                "/api/environments/staging",
                &serde_json::json!({"name": "staging", "vendor": "docker"}),
            ))
            .await
            .unwrap();
        let v: EnvironmentView = read_json(res).await;
        assert!(v.repos.is_empty(), "PUT is a full replace");
    }

    #[tokio::test]
    async fn a_run_is_listed_under_its_routine_and_nowhere_else() {
        use horsie_models::routines::RoutineRunResponse;
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        app.clone()
            .oneshot(post_json("/api/routines", &routine_body()))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/routines/nightly/run",
                &serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let run: RoutineRunResponse = read_json(res).await;
        let id = run.session.id;

        // The run is on the routine's page, which is the session list scoped
        // to it — a run is an ordinary session, so it is read like one.
        let res = app
            .clone()
            .oneshot(get("/api/sessions?routine=nightly"))
            .await
            .unwrap();
        let runs: ListSessionsResponse = read_json(res).await;
        assert_eq!(runs.sessions.len(), 1);
        assert_eq!(runs.sessions[0].id, id);

        // ...and deliberately not in the session list, though it is still
        // openable by id (that is how the run list links to it).
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let list: ListSessionsResponse = read_json(res).await;
        assert!(
            list.sessions.is_empty(),
            "a routine run must not bury the sessions somebody is having"
        );
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Running an unknown routine is a 404, not a silent no-op.
        let res = app
            .clone()
            .oneshot(post_json("/api/routines/ghost/run", &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Deleting the routine takes its runs with it: nothing else lists them.
        let res = app
            .clone()
            .oneshot(delete("/api/routines/nightly"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_agent_a_routine_uses_cannot_be_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let app = routine_app(&tmp).await;
        app.clone()
            .oneshot(post_json("/api/routines", &routine_body()))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(delete("/api/agents/reviewer"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Freed by removing the routine.
        app.clone()
            .oneshot(delete("/api/routines/nightly"))
            .await
            .unwrap();
        let res = app.oneshot(delete("/api/agents/reviewer")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn invoke_creates_a_session_and_queues_the_message() {
        use horsie_models::agents::AgentInvokeResponse;
        use horsie_models::session_api::GetSessionResponse;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        put_mock_model(&app).await;
        let body = serde_json::json!({
            "name": "reviewer", "model": "mock", "memorySpaces": ["default"]
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/agents", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        // Invoke -> 201 with the session id, immediately.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/reviewer/invoke",
                &serde_json::json!({
                    "message": "review the diff",
                    "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let invoked: AgentInvokeResponse = read_json(res).await;
        let id = invoked.session.id;

        // The session exists with the preset's model/vendor and memory spaces.
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let list: ListSessionsResponse = read_json(res).await;
        assert_eq!(list.sessions.len(), 1);
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        let detail: GetSessionResponse = read_json(res).await;
        assert_eq!(detail.session.model, "mock");
        assert_eq!(detail.session.vendor, "mock");
        assert_eq!(detail.session.memory_spaces, vec!["default".to_string()]);

        // Unknown agent -> 404; empty message -> 422.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/ghost/invoke",
                &serde_json::json!({
                    "message": "hi",
                    "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/reviewer/invoke",
                &serde_json::json!({
                    "message": "   ",
                    "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // A preset names no vendor, so what makes it invocable is whether the
        // server default is connected. Point the default at a vendor that is
        // not, and the same preset stops being invocable.
        for (uri, body) in [
            (
                "/api/config/model-providers/p",
                serde_json::json!({"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}),
            ),
            (
                "/api/config/models/mock",
                serde_json::json!({"alias": "mock", "provider": "p", "modelId": "id"}),
            ),
            // A vendor no agent answers to: still accepted, because the agent
            // may dial in later.
            (
                "/api/config/default-runtime-vendor",
                serde_json::json!({"vendor": "ghost-vendor"}),
            ),
        ] {
            let res = app.clone().oneshot(put_json(uri, &body)).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "seeding {uri}");
        }
        let res = app
            .oneshot(post_json(
                "/api/agents/reviewer/invoke",
                &serde_json::json!({"message": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_device_flow_issues_tokens_that_open_the_api() {
        use horsie_models::auth::{DeviceCodeResponse, TokenPair};
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        // The CLI starts a device authorization without any credential.
        let res = app
            .clone()
            .oneshot(post_json("/api/device/auth/code", &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let device: DeviceCodeResponse = read_json(res).await;
        assert!(device.verification_uri.ends_with("/auth/device"));
        assert!(device.verification_uri_complete.contains(&device.user_code));

        // Polling before approval is pending, not an error.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/device/auth/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let err: horsie_models::session_api::ApiError = read_json(res).await;
        assert_eq!(err.code, "authorization_pending");

        // Approving needs a logged-in browser.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/device/approve",
                &serde_json::json!({"userCode": device.user_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let approve = Request::builder()
            .method("POST")
            .uri("/api/device/approve")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": device.user_code})).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(approve).await.unwrap().status(),
            StatusCode::OK
        );

        // The next poll mints the pair immediately: an approved code skips the
        // poll floor, so this test needs no sleep.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/device/auth/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let pair: TokenPair = read_json(res).await;

        // The access token opens the API as a bearer.
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {}", pair.access_token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        // Refresh rotates, unauthenticated (the refresh token is the credential).
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/device/auth/refresh",
                &serde_json::json!({"refreshToken": pair.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let rotated: TokenPair = read_json(res).await;
        assert_ne!(rotated.refresh_token, pair.refresh_token);

        // Replaying the old refresh token is refused.
        let res = app
            .oneshot(post_json(
                "/api/device/auth/refresh",
                &serde_json::json!({"refreshToken": pair.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn denying_a_device_code_reports_access_denied_to_the_poller() {
        use horsie_models::auth::DeviceCodeResponse;
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app
            .clone()
            .oneshot(post_json("/api/device/auth/code", &serde_json::json!({})))
            .await
            .unwrap();
        let device: DeviceCodeResponse = read_json(res).await;

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let deny = Request::builder()
            .method("POST")
            .uri("/api/device/deny")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": device.user_code})).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(deny).await.unwrap().status(),
            StatusCode::OK
        );

        let res = app
            .oneshot(post_json(
                "/api/device/auth/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        let err: horsie_models::session_api::ApiError = read_json(res).await;
        assert_eq!(err.code, "access_denied");
    }

    #[tokio::test]
    async fn approving_an_unknown_user_code_is_a_404() {
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
        let req = Request::builder()
            .method("POST")
            .uri("/api/device/approve")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": "ZZZZ-ZZZZ"})).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// Bring up a real listener so vendor processes can dial a real WS upgrade.
    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("ws://{addr}/api/vendor/connect")
    }

    async fn registered_within(
        agents: &Arc<crate::runtime_vendor::RuntimeVendorRegistry>,
        name: &str,
    ) -> bool {
        for _ in 0..50 {
            if agents.connected_names().contains(&name.to_string()) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_vendor_dial_without_a_credential_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _pw) = auth_state(&tmp).await;
        let agents = services(&state).await.connected_vendors.clone();
        let url = serve(app(state)).await;

        // The dial fails at the HTTP layer — a 401, not a completed upgrade
        // that closes, which an agent would retry forever.
        let err = match crate::runtime_vendor::fake::FakeRuntimeVendor::builder("my-laptop")
            .connect(&url)
            .await
        {
            Ok(_) => panic!("a bare dial must be refused"),
            Err(e) => e,
        };
        assert!(
            err.contains("401") || err.to_lowercase().contains("http"),
            "{err}"
        );
        assert!(!registered_within(&agents, "my-laptop").await);
    }

    #[tokio::test]
    async fn a_browser_session_token_cannot_drive_a_vendor_link() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        // A valid credential of the wrong kind: right principal, but a cookie
        // has no business being a machine.
        let web = state.auth.login(&pw).await.unwrap();
        let agents = services(&state).await.connected_vendors.clone();
        let url = serve(app(state)).await;

        let outcome = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("my-laptop")
            .connect_with_token(&url, Some(&web))
            .await;
        assert!(outcome.is_err(), "a web token must not open a machine link");
        assert!(!registered_within(&agents, "my-laptop").await);
    }

    #[tokio::test]
    async fn an_agent_token_connects_and_becomes_a_selectable_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _pw) = auth_state(&tmp).await;
        let (secret, _view) = state
            .auth
            .mint_agent_token(
                "my-laptop",
                &crate::auth::Principal::User(crate::auth::UserId::new("1")),
            )
            .await
            .unwrap();
        let agents = services(&state).await.connected_vendors.clone();
        let url = serve(app(state)).await;

        let _agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("my-laptop")
            .supports_provisioning(false)
            .connect_with_token(&url, Some(&secret))
            .await
            .expect("agent connects");
        assert!(registered_within(&agents, "my-laptop").await);
    }

    #[tokio::test]
    async fn machine_tokens_are_created_listed_used_and_revoked() {
        use horsie_models::auth::{AgentTokenCreated, AgentTokenView};
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        // Minting needs a credential — otherwise anyone could mint one.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/device/tokens",
                &serde_json::json!({"label": "laptop"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let create = |body: serde_json::Value, cookie: String| {
            Request::builder()
                .method("POST")
                .uri("/api/device/tokens")
                .header("content-type", "application/json")
                .header("cookie", format!("horsie_session={cookie}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let res = app
            .clone()
            .oneshot(create(
                serde_json::json!({"label": "laptop"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: AgentTokenCreated = read_json(res).await;
        assert!(created.token.starts_with("hsk_agt_"));
        assert_eq!(created.view.label, "laptop");

        // An unlabelled token is refused.
        let res = app
            .clone()
            .oneshot(create(serde_json::json!({"label": "  "}), cookie.clone()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Listed, without the secret.
        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/device/tokens", &cookie))
            .await
            .unwrap();
        let listed: Vec<AgentTokenView> = read_json(res).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.view.id);

        // The secret is live, and restricted: `403` rather than `401` is what
        // says the token verified and was then refused this route. A machine
        // token used to answer `200` here — full session transcripts to a
        // credential minted for connecting a runtime.
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {}", created.token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        // Revoke, and it stops working.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/device/tokens/{}", created.view.id))
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {}", created.token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/device/tokens", &cookie))
            .await
            .unwrap();
        let listed: Vec<AgentTokenView> = read_json(res).await;
        assert!(listed.is_empty());

        // Revoking it again is still success — that is the state the caller
        // asked for. Revoking an id that never existed is not: it used to be a
        // 204, which reported a revocation that never happened.
        let revoke = |id: String| {
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/device/tokens/{id}"))
                .header("cookie", format!("horsie_session={cookie}"))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(revoke(created.view.id.clone()))
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            app.oneshot(revoke("no-such-token".into()))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_machine_token_reaches_nothing_but_vendor_connect() {
        // A machine token is described in the UI as being for runtime vendor
        // processes that run unattended, and `vendor_connect.rs` is explicit
        // that `/api/vendor/connect` is the one endpoint those dial. It used to
        // authenticate every route instead: reads, writes, `POST
        // /api/auth/password`, and — worst — minting further machine tokens, so
        // revoking a leaked one did not lock the holder out.
        let tmp = tempfile::tempdir().unwrap();
        let (state, _pw) = auth_state(&tmp).await;
        let (secret, _view) = state
            .auth
            .mint_agent_token(
                "vendor",
                &crate::auth::Principal::User(crate::auth::UserId::new("1")),
            )
            .await
            .unwrap();
        let app = app(state);

        let bearer = |method: &str, uri: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {secret}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        };

        for (method, uri) in [
            // A read, and the one that leaks most: whole transcripts.
            ("GET", "/api/sessions"),
            ("GET", "/api/config"),
            ("GET", "/api/agents"),
            // A write.
            ("POST", "/api/agents"),
            // Taking over the account outright.
            ("POST", "/api/auth/password"),
            // Minting a successor, which is what made revocation useless.
            ("POST", "/api/device/tokens"),
            ("GET", "/api/device/tokens"),
        ] {
            let status = app
                .clone()
                .oneshot(bearer(method, uri))
                .await
                .unwrap()
                .status();
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} must be forbidden to a machine token, got {status}"
            );
        }
    }

    fn delete_req(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    /// The status codes that carry meaning for a Terraform-style client: each
    /// resource is addressed on its own, and the failures are distinguishable.
    #[tokio::test]
    async fn per_resource_config_routes_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/model-providers/p",
                &serde_json::json!({"name": "p", "kind": "anthropic", "apiKey": "sk-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // A second provider must not disturb the first — the clobbering this
        // whole change exists to remove.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/model-providers/q",
                &serde_json::json!({"name": "q", "kind": "openai", "apiKey": "sk-y"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .clone()
            .oneshot(get("/api/config/model-providers"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let providers: serde_json::Value = read_json(res).await;
        assert_eq!(providers.as_array().unwrap().len(), 2);

        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/models/m",
                &serde_json::json!({"alias": "m", "provider": "p", "modelId": "claude-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // A model routed to a provider that does not exist is refused.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/models/bad",
                &serde_json::json!({"alias": "bad", "provider": "ghost", "modelId": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // The path is the identity; a body that disagrees is not a rename.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/config/models/m",
                &serde_json::json!({"alias": "other", "provider": "p", "modelId": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Deleting a provider cascades to the models routed through it, so the
        // model is gone without ever being named.
        let res = app
            .clone()
            .oneshot(delete_req("/api/config/model-providers/p"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .clone()
            .oneshot(delete_req("/api/config/models/m"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = app
            .clone()
            .oneshot(delete_req("/api/config/models/ghost"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
