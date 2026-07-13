//! The MCP-server service: ties the SQLite store to the MCP client and exposes
//! the operations the HTTP layer and the session agent need. Secrets stay
//! inside — `bearer_for` resolves a token (stored, or minted from the GitHub App
//! connection for `github_app`) only to build a client, never returning it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use horsie_agentcore::Toolbox;
use horsie_mcp_client::{HttpTransport, McpClient};
use horsie_models::mcp::{
    McpAuthView, McpBearerView, McpConnectResult, McpGithubAppAuth, McpNoAuth, McpServerInput,
    McpServerView,
};
use horsie_workflow::McpToolbox;

use super::oauth::{AsMetadata, McpOAuthClient, build_authorize_url, gen_pkce, gen_state};
use super::store::{McpServerRow, McpStore, StoredAuth};
use crate::github::GithubService;

/// How long an in-flight OAuth authorization is honored before it is pruned.
const PENDING_TTL: Duration = Duration::from_secs(600);

pub struct McpService {
    store: McpStore,
    /// Reused for `github_app` auth: mints the user token used as the Bearer.
    github: Arc<GithubService>,
    /// OAuth 2.1 discovery/registration/token client.
    oauth: McpOAuthClient,
    /// Transient in-flight authorizations, keyed by the opaque `state`. Holds the
    /// PKCE verifier until the browser returns to the callback. Pruned on insert.
    pending: tokio::sync::Mutex<HashMap<String, PendingAuth>>,
}

/// One in-flight OAuth authorization awaiting its callback.
struct PendingAuth {
    server: String,
    verifier: String,
    created_at: Instant,
}

impl McpService {
    pub fn new(store: McpStore, github: Arc<GithubService>) -> Self {
        Self {
            store,
            github,
            oauth: McpOAuthClient::new(),
            pending: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The configured servers, redacted for display.
    pub async fn list(&self) -> Result<Vec<McpServerView>, String> {
        Ok(self.store.list().await?.iter().map(server_view).collect())
    }

    /// Upsert a server (re-arms it: a `test` is required before it is usable).
    pub async fn upsert(&self, input: McpServerInput) -> Result<McpServerView, String> {
        Ok(server_view(&self.store.upsert(&input).await?))
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        self.store.delete(name).await
    }

    /// Smoke test: connect (`initialize` + `tools/list`) with the row's auth,
    /// persist the outcome (enabling on success), and return it.
    pub async fn test(&self, name: &str) -> Result<McpConnectResult, String> {
        let row = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("unknown MCP server '{name}'"))?;
        match self.build_toolbox(&row).await {
            Ok(tb) => {
                let count = u32::try_from(tb.specs().len()).unwrap_or(u32::MAX);
                self.store.set_status(name, true, Some(count), None).await?;
                Ok(McpConnectResult {
                    ok: true,
                    tool_count: Some(count),
                    error: None,
                })
            }
            Err(e) => {
                self.store.set_status(name, false, None, Some(&e)).await?;
                Ok(McpConnectResult {
                    ok: false,
                    tool_count: None,
                    error: Some(e),
                })
            }
        }
    }

    /// Build a live [`McpToolbox`] per named server for an agent spawn. A server
    /// that is unknown or fails to connect is skipped (logged, and its error
    /// recorded) rather than failing the whole session; only a store error
    /// propagates. Connect + `tools/list` happen here, so tools reflect the live
    /// server on each turn.
    pub async fn toolboxes_for(&self, names: &[String]) -> Result<Vec<Arc<dyn Toolbox>>, String> {
        let mut out: Vec<Arc<dyn Toolbox>> = Vec::new();
        for name in names {
            let Some(row) = self.store.get(name).await? else {
                tracing::warn!(server = %name, "session references unknown MCP server; skipping");
                continue;
            };
            match self.build_toolbox(&row).await {
                Ok(tb) => {
                    let count = u32::try_from(tb.specs().len()).unwrap_or(u32::MAX);
                    let _ = self.store.set_status(name, true, Some(count), None).await;
                    out.push(Arc::new(tb));
                }
                Err(e) => {
                    tracing::warn!(server = %name, error = %e, "MCP server connect failed; skipping");
                    let _ = self.store.set_status(name, false, None, Some(&e)).await;
                }
            }
        }
        Ok(out)
    }

    /// Begin an OAuth 2.1 authorization for an `oauth` server: resolve endpoints
    /// (cached in `oauth_meta`, else discovered), register a client via DCR when
    /// none is stored, mint PKCE + `state`, stash the verifier, and return the
    /// authorize URL the browser should be sent to.
    pub async fn connect_oauth(&self, name: &str, request_base: &str) -> Result<String, String> {
        let row = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("unknown MCP server '{name}'"))?;
        let StoredAuth::Oauth(st) = &row.auth else {
            return Err(format!("server '{name}' is not an OAuth server"));
        };

        let redirect_uri = self.callback_url(name, request_base);

        // Endpoints: cached metadata wins; otherwise discover from the resource URL.
        let mut meta = parse_meta(st.meta.as_deref());
        if meta.is_none() {
            let discovered = self.oauth.discover(&row.url).await?;
            self.store
                .save_oauth_client(
                    name,
                    st.client_id.as_deref().unwrap_or(""),
                    st.client_secret.as_ref().map(|s| s.expose()),
                    &serde_json::to_string(&discovered).map_err(|e| e.to_string())?,
                )
                .await?;
            meta = Some(discovered);
        }
        let meta = meta.ok_or_else(|| "no authorization-server endpoints".to_string())?;

        // Client: reuse a stored/manual client_id; else dynamically register.
        let client_id = match st.client_id.clone().filter(|id| !id.is_empty()) {
            Some(id) => id,
            None => {
                let reg_ep = meta.registration_endpoint.as_deref().ok_or_else(|| {
                    "server offers no dynamic registration; set a client id manually".to_string()
                })?;
                let c = self.oauth.register(reg_ep, &redirect_uri).await?;
                self.store
                    .save_oauth_client(
                        name,
                        &c.client_id,
                        c.client_secret.as_deref(),
                        &serde_json::to_string(&meta).map_err(|e| e.to_string())?,
                    )
                    .await?;
                c.client_id
            }
        };

        let pkce = gen_pkce();
        let state = gen_state();
        self.stash_pending(&state, name, &pkce.verifier).await;
        Ok(build_authorize_url(
            &meta,
            &client_id,
            &redirect_uri,
            &state,
            &pkce.challenge,
            &row.url,
        ))
    }

    /// Complete an OAuth authorization: validate `state`, exchange the code for
    /// tokens, persist them, and smoke-test the server so it becomes usable.
    pub async fn handle_oauth_callback(
        &self,
        name: &str,
        code: &str,
        state: &str,
        request_base: &str,
    ) -> Result<(), String> {
        let pending = self
            .take_pending(state)
            .await
            .ok_or_else(|| "unknown or expired authorization state".to_string())?;
        if pending.server != name {
            return Err("authorization state does not match this server".to_string());
        }
        let row = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("unknown MCP server '{name}'"))?;
        let StoredAuth::Oauth(st) = &row.auth else {
            return Err(format!("server '{name}' is not an OAuth server"));
        };
        let meta = parse_meta(st.meta.as_deref())
            .ok_or_else(|| "missing authorization-server endpoints".to_string())?;
        let client_id = st
            .client_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "no registered client".to_string())?;
        let redirect_uri = self.callback_url(name, request_base);
        let tokens = self
            .oauth
            .exchange_code(
                &meta.token_endpoint,
                &client_id,
                st.client_secret.as_ref().map(|s| s.expose()),
                code,
                &redirect_uri,
                &pending.verifier,
            )
            .await?;
        self.store
            .save_oauth_tokens(
                name,
                &tokens.access_token,
                tokens.refresh_token.as_deref(),
                tokens.expires_at.as_deref(),
            )
            .await?;
        // Enable + record tool_count via the standard smoke test.
        self.test(name).await?;
        Ok(())
    }

    /// The OAuth callback URL for a server (mirrors the github convention).
    fn callback_url(&self, name: &str, request_base: &str) -> String {
        format!(
            "{}/api/mcp/servers/{}/oauth/callback",
            request_base.trim_end_matches('/'),
            name
        )
    }

    async fn stash_pending(&self, state: &str, server: &str, verifier: &str) {
        let mut map = self.pending.lock().await;
        map.retain(|_, p| p.created_at.elapsed() < PENDING_TTL);
        map.insert(
            state.to_string(),
            PendingAuth {
                server: server.to_string(),
                verifier: verifier.to_string(),
                created_at: Instant::now(),
            },
        );
    }

    async fn take_pending(&self, state: &str) -> Option<PendingAuth> {
        let mut map = self.pending.lock().await;
        let p = map.remove(state)?;
        (p.created_at.elapsed() < PENDING_TTL).then_some(p)
    }

    /// Connect to a server and capture its tools.
    async fn build_toolbox(&self, row: &McpServerRow) -> Result<McpToolbox, String> {
        let bearer = self.bearer_for(row).await?;
        let transport = Arc::new(HttpTransport::new(row.url.clone(), bearer));
        let client = Arc::new(McpClient::new(transport));
        McpToolbox::connect(row.name.clone(), client)
            .await
            .map_err(|e| e.to_string())
    }

    /// The `Authorization: Bearer` for a server, if any. `github_app` mints the
    /// user token from the GitHub App connection.
    async fn bearer_for(&self, row: &McpServerRow) -> Result<Option<String>, String> {
        match &row.auth {
            StoredAuth::None => Ok(None),
            StoredAuth::Bearer(tok) => Ok(tok.as_ref().map(|s| s.expose().to_string())),
            StoredAuth::Oauth(st) => {
                let Some(access) = st.access_token.as_ref() else {
                    return Err(format!(
                        "MCP server '{}' is not authorized — connect it in Settings",
                        row.name
                    ));
                };
                if !needs_refresh(st.expires_at.as_deref()) {
                    return Ok(Some(access.expose().to_string()));
                }
                // Expiring: refresh when possible, else hand back the current token.
                let (Some(meta), Some(refresh), Some(client_id)) = (
                    parse_meta(st.meta.as_deref()),
                    st.refresh_token.as_ref(),
                    st.client_id.as_ref().filter(|s| !s.is_empty()),
                ) else {
                    return Ok(Some(access.expose().to_string()));
                };
                let tokens = self
                    .oauth
                    .refresh(
                        &meta.token_endpoint,
                        client_id,
                        st.client_secret.as_ref().map(|s| s.expose()),
                        refresh.expose(),
                    )
                    .await?;
                self.store
                    .save_oauth_tokens(
                        &row.name,
                        &tokens.access_token,
                        tokens.refresh_token.as_deref(),
                        tokens.expires_at.as_deref(),
                    )
                    .await?;
                Ok(Some(tokens.access_token))
            }
            StoredAuth::GithubApp => {
                let token = self.github.user_token().await?.ok_or_else(|| {
                    "GitHub is not connected — connect it in Settings to enable GitHub MCP"
                        .to_string()
                })?;
                Ok(Some(token))
            }
        }
    }
}

/// Parse the cached `oauth_meta` JSON into `AsMetadata`, if present and valid.
fn parse_meta(meta: Option<&str>) -> Option<AsMetadata> {
    meta.and_then(|m| serde_json::from_str::<AsMetadata>(m).ok())
}

/// Whether an oauth token with this stored `expires_at` (unix seconds) should be
/// refreshed now (120s skew). Absent = non-expiring → no refresh.
fn needs_refresh(expires_at: Option<&str>) -> bool {
    const SKEW: u64 = 120;
    match expires_at.and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(exp) => super::oauth::now_secs().saturating_add(SKEW) >= exp,
        None => false,
    }
}

fn server_view(row: &McpServerRow) -> McpServerView {
    let auth = match &row.auth {
        StoredAuth::None => McpAuthView::None(McpNoAuth {}),
        StoredAuth::Bearer(tok) => McpAuthView::Bearer(McpBearerView {
            has_token: tok.is_some(),
        }),
        StoredAuth::Oauth(st) => McpAuthView::OAuth(horsie_models::mcp::McpOAuthView {
            connected: st.access_token.is_some(),
            client_id: st.client_id.clone(),
            has_client_secret: st.client_secret.is_some(),
        }),
        StoredAuth::GithubApp => McpAuthView::GithubApp(McpGithubAppAuth {}),
    };
    McpServerView {
        name: row.name.clone(),
        url: row.url.clone(),
        enabled: row.enabled,
        auth,
        tool_count: row.tool_count,
        last_error: row.last_error.clone(),
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
    use crate::github::{GithubApi, GithubStore};
    use horsie_models::mcp::{McpAuthInput, McpNoAuth};
    use std::str::FromStr;

    async fn service() -> (McpService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let github = Arc::new(GithubService::new(
            GithubStore::new(pool.clone()),
            GithubApi::new(),
        ));
        (McpService::new(McpStore::new(pool), github), tmp)
    }

    /// A minimal Streamable-HTTP MCP server: JSON-RPC in, JSON out. Returns its
    /// base URL.
    async fn mock_mcp_server() -> String {
        use axum::response::{IntoResponse, Response};
        use axum::{Json, Router, http::StatusCode, routing::post};
        use serde_json::json;

        async fn handle(Json(req): Json<serde_json::Value>) -> Response {
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let Some(id) = req.get("id").cloned() else {
                // A notification (e.g. notifications/initialized) — no response body.
                return StatusCode::ACCEPTED.into_response();
            };
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": { "name": "mock", "version": "0" }
                }),
                "tools/list" => json!({ "tools": [
                    { "name": "echo", "description": "echo", "inputSchema": { "type": "object" } },
                    { "name": "ping", "description": "ping", "inputSchema": { "type": "object" } }
                ] }),
                "tools/call" => {
                    json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false })
                }
                _ => json!({}),
            };
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
        }

        let app = Router::new().route("/", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn none_input(name: &str, url: &str) -> McpServerInput {
        McpServerInput {
            name: name.into(),
            url: url.into(),
            auth: McpAuthInput::None(McpNoAuth {}),
        }
    }

    /// A mock authorization server: DCR + a token endpoint (metadata routes are
    /// unused here because the test supplies endpoints manually).
    async fn mock_as() -> String {
        use axum::{Json, Router, routing::post};
        use serde_json::json;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/register",
                post(|| async { Json(json!({ "client_id": "dcr-client" })) }),
            )
            .route(
                "/token",
                post(
                    |axum::extract::Form(f): axum::extract::Form<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let at = if f.get("grant_type").map(String::as_str) == Some("refresh_token")
                        {
                            "at-2"
                        } else {
                            "at-1"
                        };
                        Json(json!({
                            "access_token": at,
                            "refresh_token": "rt-1",
                            "token_type": "bearer",
                            "expires_in": 3600
                        }))
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn oauth_connect_then_callback_enables_the_server() {
        let (svc, _t) = service().await;
        let mcp = mock_mcp_server().await; // the MCP endpoint (tools live here)
        let as_base = mock_as().await; // the authorization server

        // An oauth row with a pre-registered client + manually-set endpoints
        // (skip live discovery by seeding the endpoints through the input).
        svc.upsert(McpServerInput {
            name: "generic".into(),
            url: mcp.clone(),
            auth: McpAuthInput::OAuth(horsie_models::mcp::McpOAuthInput {
                client_id: Some("cid".into()),
                client_secret: None,
                authorization_endpoint: Some(format!("{as_base}/authorize")),
                token_endpoint: Some(format!("{as_base}/token")),
                registration_endpoint: None,
            }),
        })
        .await
        .unwrap();

        // Connect: returns an authorize URL carrying our client + a state param.
        let url = svc.connect_oauth("generic", "http://host").await.unwrap();
        assert!(url.starts_with(&format!("{as_base}/authorize?")), "{url}");
        assert!(url.contains("code_challenge_method=S256"));
        let state = url
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        // Callback: exchange the code, persist tokens, smoke-test → enabled.
        svc.handle_oauth_callback("generic", "the-code", &state, "http://host")
            .await
            .unwrap();
        let view = svc.list().await.unwrap();
        assert!(
            view[0].enabled,
            "server should be enabled after a successful callback"
        );
        match &view[0].auth {
            McpAuthView::OAuth(o) => assert!(o.connected, "connected once a token is stored"),
            other => panic!("expected oauth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oauth_callback_with_unknown_state_is_rejected() {
        let (svc, _t) = service().await;
        svc.upsert(McpServerInput {
            name: "g".into(),
            url: "http://127.0.0.1:0/".into(),
            auth: McpAuthInput::OAuth(horsie_models::mcp::McpOAuthInput {
                client_id: Some("cid".into()),
                client_secret: None,
                authorization_endpoint: Some("http://as/authorize".into()),
                token_endpoint: Some("http://as/token".into()),
                registration_endpoint: None,
            }),
        })
        .await
        .unwrap();
        let err = svc
            .handle_oauth_callback("g", "code", "never-issued", "http://host")
            .await
            .unwrap_err();
        assert!(err.contains("state"), "{err}");
    }

    #[tokio::test]
    async fn test_connects_live_server_and_records_status() {
        let (svc, _t) = service().await;
        let url = mock_mcp_server().await;
        svc.upsert(none_input("mock", &url)).await.unwrap();

        let result = svc.test("mock").await.unwrap();
        assert!(result.ok, "connect should succeed: {result:?}");
        assert_eq!(result.tool_count, Some(2));

        // Status is persisted and surfaced in the view.
        let view = svc.list().await.unwrap();
        assert_eq!(view.len(), 1);
        assert!(view[0].enabled);
        assert_eq!(view[0].tool_count, Some(2));
        assert!(view[0].last_error.is_none());
    }

    #[tokio::test]
    async fn toolboxes_for_returns_namespaced_live_tools() {
        let (svc, _t) = service().await;
        let url = mock_mcp_server().await;
        svc.upsert(none_input("mock", &url)).await.unwrap();

        let boxes = svc.toolboxes_for(&["mock".into()]).await.unwrap();
        assert_eq!(boxes.len(), 1);
        let mut names: Vec<String> = boxes[0].specs().into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(names, vec!["mcp__mock__echo", "mcp__mock__ping"]);
    }

    #[tokio::test]
    async fn test_records_error_for_unreachable_server() {
        let (svc, _t) = service().await;
        // Nothing is listening here.
        svc.upsert(none_input("dead", "http://127.0.0.1:0/"))
            .await
            .unwrap();
        let result = svc.test("dead").await.unwrap();
        assert!(!result.ok);
        assert!(result.error.is_some());
        let view = svc.list().await.unwrap();
        assert!(!view[0].enabled);
        assert!(view[0].last_error.is_some());
    }

    #[tokio::test]
    async fn toolboxes_for_skips_unknown_names() {
        let (svc, _t) = service().await;
        let boxes = svc.toolboxes_for(&["ghost".into()]).await.unwrap();
        assert!(boxes.is_empty());
    }
}
