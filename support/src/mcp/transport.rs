use crate::mcp::error::McpError;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Bounds TCP + TLS setup against an MCP endpoint.
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Bounds idle time between reads. MCP calls are request/response, so a silent
/// server is stalled rather than slow.
const READ_TIMEOUT_SECS: u64 = 30;

/// The wire seam under [`McpClient`](crate::mcp::McpClient): issues JSON-RPC requests
/// and notifications. Mockable for tests; [`HttpTransport`] is the live impl.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and return its `result` value.
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Send a JSON-RPC notification (no id, no response).
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;
}

/// Supplies the `Authorization: Bearer` for an MCP connection, refreshably.
/// `force = true` (issued by the transport after a 401) must attempt a fresh
/// token, bypassing any cache. `Ok(None)` means "no auth" (public server).
#[async_trait]
pub trait BearerProvider: Send + Sync {
    async fn bearer(&self, force: bool) -> Result<Option<String>, McpError>;
}

/// MCP Streamable HTTP transport: POSTs JSON-RPC to a single endpoint and reads
/// back either a JSON body or an SSE stream, carrying the `Mcp-Session-Id`
/// across requests. The bearer comes from a [`BearerProvider`], so an expired
/// token is force-refreshed and the request retried once on a `401`.
pub struct HttpTransport {
    endpoint: String,
    auth: Arc<dyn BearerProvider>,
    /// Headers the caller declared, sent on every request exactly as given.
    /// A plugin's `.mcp.json` carries its token this way, and it is not always
    /// an `Authorization: Bearer` — rewriting one into that shape is how a
    /// working declaration stops working.
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(endpoint: String, auth: Arc<dyn BearerProvider>) -> Self {
        Self::with_headers(endpoint, auth, Vec::new())
    }

    /// As [`HttpTransport::new`], plus static headers on every request. A
    /// resolved bearer still wins the `Authorization` slot: it is refreshable
    /// and a declared one is not.
    pub fn with_headers(
        endpoint: String,
        auth: Arc<dyn BearerProvider>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            endpoint,
            auth,
            headers,
            // Bounded like every other HTTP client in the repo. An MCP server that
            // accepts the connection and then goes silent used to hang the run in
            // a place `Stop` cannot reach (#61 item 5); the deadline is what makes
            // that failure surface as an error instead of a wedged session.
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .read_timeout(Duration::from_secs(READ_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
        }
    }

    /// Build a POST for `body` with the given bearer (if any) and the session
    /// header. The session-id lock is released before the request is awaited.
    fn build(&self, body: &Value, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(body);
        // A resolved bearer owns the `Authorization` slot — it refreshes and a
        // declared one cannot — so the declared header is dropped rather than
        // sent alongside it. `header` appends, and a server reading the first
        // value would otherwise see the stale one.
        let bearer_wins = token.is_some();
        for (k, v) in &self.headers {
            if bearer_wins && k.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(k, v);
        }
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let sid = self.session_id.lock().ok().and_then(|g| g.clone());
        if let Some(sid) = sid {
            req = req.header("mcp-session-id", sid);
        }
        req
    }

    /// Remember the server-assigned session id, if any.
    fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(v) = resp.headers().get("mcp-session-id")
            && let Ok(s) = v.to_str()
            && let Ok(mut g) = self.session_id.lock()
        {
            *g = Some(s.to_string());
        }
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let token = self.auth.bearer(false).await?;
        let mut resp = self
            .build(&body, token.as_deref())
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        self.capture_session(&resp);

        // On 401, force one token refresh and retry once (only if it changed).
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let fresh = self.auth.bearer(true).await?;
            if fresh.is_some() && fresh != token {
                resp = self
                    .build(&body, fresh.as_deref())
                    .send()
                    .await
                    .map_err(|e| McpError::Transport(e.to_string()))?;
                self.capture_session(&resp);
            }
        }

        let status = resp.status();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let ctype = header("content-type").unwrap_or_default();
        // Captured before the body is consumed: this is what says *how* to
        // authenticate, and it is the only actionable part of a 401.
        let www_authenticate = header("www-authenticate");
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::Unauthorized { www_authenticate });
        }
        if !status.is_success() {
            return Err(McpError::Transport(format!("http {status}: {text}")));
        }
        let msg = if ctype.contains("text/event-stream") {
            parse_sse_response(&text)?
        } else {
            serde_json::from_str::<Value>(&text).map_err(|e| McpError::Protocol(e.to_string()))?
        };
        extract_result(msg)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let token = self.auth.bearer(false).await?;
        let resp = self
            .build(&body, token.as_deref())
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        self.capture_session(&resp);
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(McpError::Transport(format!("http {status}")))
        }
    }
}

/// Parse a Streamable-HTTP SSE body: concatenate the `data:` lines of each
/// event and return the first JSON-RPC message carrying a `result` or `error`.
pub(crate) fn parse_sse_response(body: &str) -> Result<Value, McpError> {
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
            continue;
        }
        if line.trim().is_empty() && !data.is_empty() {
            if let Some(v) = as_jsonrpc_response(&data) {
                return Ok(v);
            }
            data.clear();
        }
    }
    if let Some(v) = as_jsonrpc_response(&data) {
        return Ok(v);
    }
    Err(McpError::Protocol(
        "no JSON-RPC response in SSE stream".to_string(),
    ))
}

/// Parse `data` as JSON and keep it only if it looks like a JSON-RPC response.
fn as_jsonrpc_response(data: &str) -> Option<Value> {
    if data.is_empty() {
        return None;
    }
    let v = serde_json::from_str::<Value>(data).ok()?;
    (v.get("result").is_some() || v.get("error").is_some()).then_some(v)
}

/// Turn a JSON-RPC response object into its `result`, mapping an `error`.
fn extract_result(msg: Value) -> Result<Value, McpError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Err(McpError::Rpc { code, message });
    }
    match msg.get("result") {
        Some(r) => Ok(r.clone()),
        None => Err(McpError::Protocol("response missing result".to_string())),
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

    #[test]
    fn parse_sse_extracts_the_response_event() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = parse_sse_response(body).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn parse_sse_joins_multiline_data() {
        let body = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1,\"result\":42}\n\n";
        let v = parse_sse_response(body).unwrap();
        assert_eq!(v["result"], json!(42));
    }

    #[test]
    fn parse_sse_without_a_response_errors() {
        let body = "event: ping\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\n";
        assert!(matches!(
            parse_sse_response(body),
            Err(McpError::Protocol(_))
        ));
    }

    #[test]
    fn extract_result_maps_rpc_error() {
        let msg = json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}});
        match extract_result(msg) {
            Err(McpError::Rpc { code, message }) => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("expected rpc error, got {other:?}"),
        }
    }

    use std::sync::atomic::AtomicBool;

    /// A provider that serves "old" until it sees a `force` call, then "new".
    struct SwitchingProvider {
        forced: AtomicBool,
    }

    #[async_trait]
    impl BearerProvider for SwitchingProvider {
        async fn bearer(&self, force: bool) -> Result<Option<String>, McpError> {
            if force {
                self.forced.store(true, Ordering::SeqCst);
            }
            let tok = if self.forced.load(Ordering::SeqCst) {
                "new"
            } else {
                "old"
            };
            Ok(Some(tok.to_string()))
        }
    }

    /// A provider whose token never changes, even on force.
    struct StaticProvider;

    #[async_trait]
    impl BearerProvider for StaticProvider {
        async fn bearer(&self, _force: bool) -> Result<Option<String>, McpError> {
            Ok(Some("old".to_string()))
        }
    }

    /// Mock MCP server: 401 unless `Authorization: Bearer new`, else a JSON-RPC ok.
    async fn mock_needs_new_token() -> String {
        use axum::response::{IntoResponse, Response};
        use axum::{Json, Router, http::HeaderMap, http::StatusCode, routing::post};
        async fn handle(headers: HeaderMap, Json(req): Json<Value>) -> Response {
            let ok =
                headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer new");
            if !ok {
                return (StatusCode::UNAUTHORIZED, "expired").into_response();
            }
            let id = req.get("id").cloned().unwrap_or(json!(1));
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })).into_response()
        }
        let app = Router::new().route("/", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn request_refreshes_and_retries_on_401() {
        let url = mock_needs_new_token().await;
        let provider = Arc::new(SwitchingProvider {
            forced: AtomicBool::new(false),
        });
        let t = HttpTransport::new(url, provider);
        let v = t.request("tools/call", json!({})).await.unwrap();
        assert_eq!(v["ok"], json!(true));
    }

    /// A refresh that changes nothing leaves the 401 to the caller — and it
    /// arrives as `Unauthorized`, keeping the challenge that says where to go
    /// and get a token, rather than flattened into a transport error.
    #[tokio::test]
    async fn request_propagates_401_when_token_unchanged() {
        let url = mock_needs_new_token().await;
        let t = HttpTransport::new(url, Arc::new(StaticProvider));
        let err = t.request("tools/call", json!({})).await.unwrap_err();
        let McpError::Unauthorized { www_authenticate } = err else {
            panic!("{err:?}");
        };
        assert!(
            www_authenticate
                .as_deref()
                .is_none_or(|c| c.starts_with("Bearer")),
            "{www_authenticate:?}"
        );
    }

    /// Declared headers reach the wire as declared: a header that is not
    /// `Authorization` survives, and a non-`Bearer` scheme is not rewritten.
    #[tokio::test]
    async fn declared_headers_reach_the_request_unaltered() {
        async fn handle(headers: axum::http::HeaderMap) -> impl axum::response::IntoResponse {
            let seen = |k: &str| {
                headers
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            };
            axum::Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "key": seen("x-api-key"), "auth": seen("authorization") }
            }))
        }
        let app = axum::Router::new().route("/", axum::routing::post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let t = HttpTransport::with_headers(
            format!("http://{addr}/"),
            Arc::new(NoProvider),
            vec![
                ("X-API-Key".to_string(), "k".to_string()),
                ("Authorization".to_string(), "token abc".to_string()),
            ],
        );
        let v = t.request("initialize", json!({})).await.unwrap();
        assert_eq!(v["key"], json!("k"));
        assert_eq!(v["auth"], json!("token abc"));
    }

    /// A resolved bearer still wins the `Authorization` slot: it refreshes and
    /// a declared one cannot.
    #[tokio::test]
    async fn a_resolved_bearer_replaces_a_declared_authorization() {
        async fn handle(headers: axum::http::HeaderMap) -> impl axum::response::IntoResponse {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            axum::Json(json!({ "jsonrpc": "2.0", "id": 1, "result": { "auth": auth } }))
        }
        let app = axum::Router::new().route("/", axum::routing::post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let t = HttpTransport::with_headers(
            format!("http://{addr}/"),
            Arc::new(StaticProvider),
            vec![("Authorization".to_string(), "token declared".to_string())],
        );
        let v = t.request("initialize", json!({})).await.unwrap();
        assert_eq!(v["auth"], json!("Bearer old"));
    }

    /// No credential at all: a plugin server that authenticates with a header,
    /// or none.
    struct NoProvider;

    #[async_trait]
    impl BearerProvider for NoProvider {
        async fn bearer(&self, _force: bool) -> Result<Option<String>, McpError> {
            Ok(None)
        }
    }
}
