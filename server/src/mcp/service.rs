//! The MCP-server service: ties the SQLite store to the MCP client and exposes
//! the operations the HTTP layer and the session agent need. Secrets stay
//! inside — `bearer_for` resolves a token (stored, or minted from the GitHub App
//! connection for `github_app`) only to build a client, never returning it.

use std::sync::Arc;

use horsie_agentcore::Toolbox;
use horsie_mcp_client::{HttpTransport, McpClient};
use horsie_models::mcp::{
    McpAuthView, McpBearerView, McpConnectResult, McpGithubAppAuth, McpNoAuth, McpServerInput,
    McpServerView,
};
use horsie_workflow::McpToolbox;

use super::store::{McpServerRow, McpStore, StoredAuth};
use crate::github::GithubService;

pub struct McpService {
    store: McpStore,
    /// Reused for `github_app` auth: mints the user token used as the Bearer.
    github: Arc<GithubService>,
}

impl McpService {
    pub fn new(store: McpStore, github: Arc<GithubService>) -> Self {
        Self { store, github }
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
                // Refresh-at-use is added in the OAuth-orchestration task; for now
                // hand back the stored token, or fail if not yet authorized.
                let access = st.access_token.as_ref().ok_or_else(|| {
                    format!(
                        "MCP server '{}' is not authorized — connect it in Settings",
                        row.name
                    )
                })?;
                Ok(Some(access.expose().to_string()))
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
