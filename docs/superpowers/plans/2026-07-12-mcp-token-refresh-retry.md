# MCP mid-turn token refresh & 401 retry — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A `tools/call` that 401s mid-turn triggers one forced token refresh + retry, so OAuth/`github_app` MCP servers self-heal within a turn.

**Architecture:** `HttpTransport` takes an `Arc<dyn BearerProvider>` (refreshable) instead of a static bearer; on 401 it force-refreshes once and retries. Server-side, the provider owns cheap clones of the store/oauth/github collaborators and reuses the existing per-auth-kind resolution with a `force` path.

**Tech Stack:** Rust (async-trait, reqwest, serde_json, sqlx). No new deps.

Spec: `docs/superpowers/specs/2026-07-12-mcp-token-refresh-retry-design.md`.

## Global Constraints

- Rust 1.96.0; CI = `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --workspace --all-features`, `cargo deny check`, TS drift (unaffected — no `.fl`/web change).
- Production code denies `unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm`; test modules opt out via the repo's `#[cfg(test)] mod tests` `#[allow(...)]` block.
- No new deps. Commit `Cargo.lock` if it changes (it shouldn't). No AI attribution; short subjects. Branch `mcp-token-retry`.

## File Structure

- `mcp-client/src/transport.rs` — **modify**: `BearerProvider` trait; `HttpTransport` holds `auth: Arc<dyn BearerProvider>`; `request()` retry.
- `mcp-client/src/lib.rs` — **modify**: export `BearerProvider`.
- `server/src/mcp/store.rs` — **modify**: `#[derive(Clone)]` on `McpStore`.
- `server/src/mcp/oauth.rs` — **modify**: `#[derive(Clone)]` on `McpOAuthClient`.
- `server/src/mcp/service.rs` — **modify**: `resolve_bearer(..., force)`, `McpServerBearerProvider`, `build_toolbox` uses it, remove `bearer_for`.
- `server/src/github/service.rs` — **modify**: `user_token(force: bool)`.

---

## Task 1: `mcp-client` — `BearerProvider` + transport 401 retry

**Files:** modify `mcp-client/src/transport.rs`, `mcp-client/src/lib.rs`.

**Interfaces produced:**
- `#[async_trait] pub trait BearerProvider: Send + Sync { async fn bearer(&self, force: bool) -> Result<Option<String>, McpError>; }`
- `HttpTransport::new(endpoint: String, auth: Arc<dyn BearerProvider>) -> Self`

- [ ] **Step 1: Failing test — transport refreshes + retries once on 401.**

Add to `transport.rs` tests (the module already `#[allow(...)]`s the lint set). Add `use std::sync::Arc;` inside the test module if not present.

```rust
struct SwitchingProvider {
    // returns "old" until force=true is seen, then "new"
    forced: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl BearerProvider for SwitchingProvider {
    async fn bearer(&self, force: bool) -> Result<Option<String>, McpError> {
        if force {
            self.forced.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(Some(
            if self.forced.load(std::sync::atomic::Ordering::SeqCst) { "new" } else { "old" }.into(),
        ))
    }
}

/// Mock MCP server: 401 unless `Authorization: Bearer new`, else a JSON-RPC ok.
async fn mock_needs_new_token() -> String {
    use axum::response::{IntoResponse, Response};
    use axum::{Json, Router, http::StatusCode, http::HeaderMap, routing::post};
    use serde_json::json;
    async fn handle(headers: HeaderMap, Json(req): Json<serde_json::Value>) -> Response {
        let ok = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            == Some("Bearer new");
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
        forced: std::sync::atomic::AtomicBool::new(false),
    });
    let t = HttpTransport::new(url, provider);
    let v = t.request("tools/call", serde_json::json!({})).await.unwrap();
    assert_eq!(v["ok"], serde_json::json!(true));
}

/// A provider whose token never changes → the 401 propagates (no infinite retry).
struct StaticProvider;
#[async_trait]
impl BearerProvider for StaticProvider {
    async fn bearer(&self, _force: bool) -> Result<Option<String>, McpError> {
        Ok(Some("old".into()))
    }
}

#[tokio::test]
async fn request_propagates_401_when_token_unchanged() {
    let url = mock_needs_new_token().await;
    let t = HttpTransport::new(url, Arc::new(StaticProvider));
    let err = t.request("tools/call", serde_json::json!({})).await.unwrap_err();
    assert!(matches!(err, McpError::Transport(_)));
}
```

`mock_needs_new_token` uses `axum` — add `axum` to `mcp-client/[dev-dependencies]` (workspace) if not already present (the crate's other tests may not use it). Check `mcp-client/Cargo.toml`; add `axum = { workspace = true }` under `[dev-dependencies]` if missing.

- [ ] **Step 2: Run — fails to compile (`BearerProvider` missing, `new` signature).**

Run: `cargo test -p horsie-mcp-client transport 2>&1 | tail -15`
Expected: FAIL (no `BearerProvider`).

- [ ] **Step 3: Add the trait, change the struct, rewrite `request()`.**

Replace the `use` header + struct + `HttpTransport` impl block in `transport.rs`:

```rust
use crate::error::McpError;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Supplies the `Authorization: Bearer` for an MCP connection, refreshably.
/// `force = true` (after a 401) must attempt a fresh token, bypassing any cache.
/// `Ok(None)` means "no auth" (public server).
#[async_trait]
pub trait BearerProvider: Send + Sync {
    async fn bearer(&self, force: bool) -> Result<Option<String>, McpError>;
}
```

Keep the existing `McpTransport` trait doc/definition as-is. Change `HttpTransport`:

```rust
pub struct HttpTransport {
    endpoint: String,
    auth: Arc<dyn BearerProvider>,
    http: reqwest::Client,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(endpoint: String, auth: Arc<dyn BearerProvider>) -> Self {
        Self {
            endpoint,
            auth,
            http: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
        }
    }

    /// Build a POST for `body` with the given bearer (if any) and the session id.
    fn build(&self, body: &Value, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(body);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let sid = self.session_id.lock().ok().and_then(|g| g.clone());
        if let Some(sid) = sid {
            req = req.header("mcp-session-id", sid);
        }
        req
    }

    fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(v) = resp.headers().get("mcp-session-id")
            && let Ok(s) = v.to_str()
            && let Ok(mut g) = self.session_id.lock()
        {
            *g = Some(s.to_string());
        }
    }
}
```

Rewrite the `McpTransport for HttpTransport` `request()` (keep `notify()` but fetch its token via the provider):

```rust
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

        // On 401, force one token refresh and retry once (if the token changed).
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
```

- [ ] **Step 4: Export `BearerProvider` from `lib.rs`.**

In `mcp-client/src/lib.rs`, add `BearerProvider` to the `pub use transport::{...}` list (next to `HttpTransport`/`McpTransport`).

- [ ] **Step 5: Run the transport tests + clippy.**

Run: `cargo test -p horsie-mcp-client 2>&1 | tail -15`
Expected: PASS (new retry tests + existing SSE-parser tests).
Run: `cargo clippy -p horsie-mcp-client --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 6: Commit.**

```bash
git add mcp-client/src/transport.rs mcp-client/src/lib.rs mcp-client/Cargo.toml Cargo.lock
git commit -m "mcp-client: refreshable bearer provider + 401 retry in HttpTransport"
```

---

## Task 2: `server` — provider impl, `force` refresh, derive Clone

**Files:** modify `server/src/mcp/store.rs`, `server/src/mcp/oauth.rs`, `server/src/mcp/service.rs`, `server/src/github/service.rs`.

**Interfaces produced:**
- `GithubService::user_token(&self, force: bool) -> Result<Option<String>, String>`
- (private) `McpServerBearerProvider: BearerProvider`, `resolve_bearer(&McpStore, &McpOAuthClient, &Arc<GithubService>, &McpServerRow, bool)`.

- [ ] **Step 1: Derive `Clone`.**

`server/src/mcp/store.rs`: `#[derive(Clone)] pub struct McpStore { pool: SqlitePool }` (add `Clone` to the existing derive/attribute — the struct currently has no derive; add `#[derive(Clone)]`).
`server/src/mcp/oauth.rs`: add `#[derive(Clone)]` to `pub struct McpOAuthClient { http: reqwest::Client }`.

- [ ] **Step 2: `GithubService::user_token(force)`.**

In `server/src/github/service.rs`, change the signature to
`pub async fn user_token(&self, force: bool) -> Result<Option<String>, String>` and gate the early return on `!force`:

```rust
if !force && !needs_refresh(creds.expires_at.as_deref()) {
    return Ok(Some(creds.access_token.expose().to_string()));
}
```

The rest (refresh via `refresh_token`, else return current) is unchanged. If a caller elsewhere breaks, update it — but the only non-test caller is the `github_app` arm below.

- [ ] **Step 3: Failing test — `resolve_bearer(force=true)` refreshes a not-near-expiry OAuth token.**

Add to `service.rs` tests (reuse the existing `mock_as()` and `service()` helpers). Seed an oauth row with a far-future `expires_at` + tokens/meta via the store, then assert `resolve_bearer` with `force=true` returns the refreshed access token (`at-2` from the mock AS) and persists it:

```rust
#[tokio::test]
async fn resolve_bearer_force_refreshes_even_when_not_near_expiry() {
    let (svc, _t) = service().await;
    let as_base = mock_as().await;
    // An oauth row, already "connected", token valid far into the future.
    svc.upsert(McpServerInput {
        name: "o".into(),
        url: "https://mcp.example/".into(),
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
    svc.store
        .save_oauth_tokens("o", "at-old", Some("rt-1"), Some("9999999999"))
        .await
        .unwrap();
    let row = svc.store.get("o").await.unwrap().unwrap();

    // force=false → not near expiry → keep the current token.
    let keep = resolve_bearer(&svc.store, &svc.oauth, &svc.github, &row, false)
        .await
        .unwrap();
    assert_eq!(keep.as_deref(), Some("at-old"));

    // force=true → refresh via the mock AS even though it is not near expiry.
    let fresh = resolve_bearer(&svc.store, &svc.oauth, &svc.github, &row, true)
        .await
        .unwrap();
    assert_eq!(fresh.as_deref(), Some("at-2"));
    // Persisted.
    let row = svc.store.get("o").await.unwrap().unwrap();
    let horsie_models::mcp::McpServerView { .. } = svc.list().await.unwrap().remove(0);
    let _ = row;
}
```

(The test reaches `svc.store`/`svc.oauth`/`svc.github` — keep those fields accessible to the in-module test; they already are, being private fields used within `mod tests` via `super`.)

- [ ] **Step 4: Run — fails (no `resolve_bearer`).**

Run: `cargo test -p horsie-server mcp::service::tests::resolve_bearer_force 2>&1 | tail -15`
Expected: FAIL (`cannot find function resolve_bearer`).

- [ ] **Step 5: Extract `resolve_bearer` (from `bearer_for` + force) and add the provider.**

In `service.rs`, replace `bearer_for` with a free function `resolve_bearer` (same per-auth-kind logic; `oauth` arm refreshes when `force || needs_refresh(...)`):

```rust
/// Resolve the `Authorization: Bearer` for a server row. `force` (after a 401)
/// refreshes OAuth/github tokens even when not near expiry.
async fn resolve_bearer(
    store: &McpStore,
    oauth: &McpOAuthClient,
    github: &GithubService,
    row: &McpServerRow,
    force: bool,
) -> Result<Option<String>, String> {
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
            if !force && !needs_refresh(st.expires_at.as_deref()) {
                return Ok(Some(access.expose().to_string()));
            }
            let (Some(meta), Some(refresh), Some(client_id)) = (
                parse_meta(st.meta.as_deref()),
                st.refresh_token.as_ref(),
                st.client_id.as_ref().filter(|s| !s.is_empty()),
            ) else {
                return Ok(Some(access.expose().to_string()));
            };
            let tokens = oauth
                .refresh(
                    &meta.token_endpoint,
                    client_id,
                    st.client_secret.as_ref().map(|s| s.expose()),
                    refresh.expose(),
                )
                .await?;
            store
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
            let token = github.user_token(force).await?.ok_or_else(|| {
                "GitHub is not connected — connect it in Settings to enable GitHub MCP".to_string()
            })?;
            Ok(Some(token))
        }
    }
}
```

Add the provider (near the bottom of `service.rs`), plus `use horsie_mcp_client::{BearerProvider, McpError};` and `use async_trait::async_trait;` at the top (join existing imports):

```rust
/// A per-server [`BearerProvider`] for [`HttpTransport`]: resolves the token from
/// the store once per turn (cached), and force-refreshes it after a 401.
struct McpServerBearerProvider {
    store: McpStore,
    oauth: McpOAuthClient,
    github: Arc<GithubService>,
    server: String,
    cached: tokio::sync::Mutex<Option<Option<String>>>,
}

#[async_trait]
impl BearerProvider for McpServerBearerProvider {
    async fn bearer(&self, force: bool) -> Result<Option<String>, McpError> {
        let mut cache = self.cached.lock().await;
        if !force && let Some(tok) = cache.as_ref() {
            return Ok(tok.clone());
        }
        let row = self
            .store
            .get(&self.server)
            .await
            .map_err(McpError::Transport)?
            .ok_or_else(|| McpError::Transport(format!("unknown MCP server '{}'", self.server)))?;
        let tok = resolve_bearer(&self.store, &self.oauth, &self.github, &row, force)
            .await
            .map_err(McpError::Transport)?;
        *cache = Some(tok.clone());
        Ok(tok)
    }
}
```

Rewrite `build_toolbox` to build the provider instead of a static bearer:

```rust
async fn build_toolbox(&self, row: &McpServerRow) -> Result<McpToolbox, String> {
    let provider = Arc::new(McpServerBearerProvider {
        store: self.store.clone(),
        oauth: self.oauth.clone(),
        github: self.github.clone(),
        server: row.name.clone(),
        cached: tokio::sync::Mutex::new(None),
    });
    let transport = Arc::new(HttpTransport::new(row.url.clone(), provider));
    let client = Arc::new(McpClient::new(transport));
    McpToolbox::connect(row.name.clone(), client)
        .await
        .map_err(|e| e.to_string())
}
```

Delete the old `bearer_for` method (its logic now lives in `resolve_bearer`). Update the module doc comment on `service.rs` that mentions `bearer_for` → `resolve_bearer`.

- [ ] **Step 6: Run the server tests + clippy.**

Run: `cargo test -p horsie-server mcp:: 2>&1 | tail -20`
Expected: PASS (new force-refresh test + all existing store/oauth/service tests, incl. the OAuth connect/callback flow which still exercises `build_toolbox`).
Run: `cargo clippy -p horsie-server --all-targets --all-features -- -D warnings 2>&1 | tail -8`
Expected: clean. (Watch for a non-exhaustive match — `resolve_bearer` spells every `StoredAuth` variant.)

- [ ] **Step 7: Commit.**

```bash
git add server/src/mcp/store.rs server/src/mcp/oauth.rs server/src/mcp/service.rs server/src/github/service.rs
git commit -m "mcp: force-refresh + 401 retry via a per-server bearer provider"
```

---

## Task 3: full gate + PR

- [ ] **Step 1:** `cargo fmt --all && cargo fmt --all -- --check`.
- [ ] **Step 2:** `cargo clippy --locked --all-targets --all-features -- -D warnings` — clean.
- [ ] **Step 3:** `cargo test --locked --workspace --all-features` — pass.
- [ ] **Step 4:** `cargo deny check advisories bans licenses sources` — pass (no new deps; note if `cargo-deny` absent).
- [ ] **Step 5:** commit any fmt changes; `git push -u origin mcp-token-retry`; `gh pr create` describing the mid-turn 401 retry; verify CI green.

## Self-review

- **Spec coverage:** BearerProvider + transport retry (Task 1); force path + provider + derive-Clone + `user_token(force)` (Task 2); non-goals (no caching, single retry, 401-only) honored.
- **Placeholders:** none.
- **Type consistency:** `BearerProvider::bearer(force) -> Result<Option<String>, McpError>` defined in Task 1 is implemented in Task 2; `resolve_bearer` signature is identical between its definition and the provider/test call sites; `user_token(force)` matches its single caller.
</content>
