use crate::error::McpError;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The wire seam under [`McpClient`](crate::McpClient): issues JSON-RPC requests
/// and notifications. Mockable for tests; [`HttpTransport`] is the live impl.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and return its `result` value.
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Send a JSON-RPC notification (no id, no response).
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;
}

/// MCP Streamable HTTP transport: POSTs JSON-RPC to a single endpoint and reads
/// back either a JSON body or an SSE stream, carrying the `Mcp-Session-Id`
/// across requests. An optional bearer token is injected as `Authorization`.
pub struct HttpTransport {
    endpoint: String,
    bearer: Option<String>,
    http: reqwest::Client,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(endpoint: String, bearer: Option<String>) -> Self {
        Self {
            endpoint,
            bearer,
            http: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
        }
    }

    /// Build a POST for `body`, adding auth and session headers. The session-id
    /// lock is released before the request is awaited.
    fn build(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(body);
        if let Some(token) = &self.bearer {
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
        let resp = self
            .build(&body)
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        self.capture_session(&resp);
        let status = resp.status();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
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
        let resp = self
            .build(&body)
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
}
