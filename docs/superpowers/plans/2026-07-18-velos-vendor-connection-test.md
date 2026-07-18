# Velos Vendor Connection Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an on-demand "Test connection" check for configured velos vendors — reachability + token validity, checked live against velos's existing `GET /auth/v1/me` — surfaced as a per-row button in the Settings UI plus an automatic check right after saving.

**Architecture:** A new `VelosClient::whoami()` inherent method calls velos's existing (unmodified) `/auth/v1/me` endpoint. A new `ConfigStore::test_vendor(name)` reads the vendor's row straight from the DB, builds a throwaway `VelosClient`, and calls `whoami()` — read-only, no persistence. A new `POST /api/config/vendors/:name/test` HTTP route exposes it, mirroring the existing `POST /api/mcp/servers/:name/test` shape exactly (always `200`, `ok: false` + `error` for a failed check, not an HTTP error). The web Settings page adds a per-row button plus fires the same check automatically right after a successful save.

**Tech Stack:** Rust (axum, sqlx/SQLite, reqwest), fluorite schema codegen (Rust + TypeScript), React 19 + TanStack Query (`clients/web`).

## Global Constraints

- No velos repo changes — this is horsie-only. (Spec: Summary, Goals.)
- `whoami()` never touches `ContainerApi`, `VelosVendor`, or any live-reconfigure machinery — it's a fresh, ephemeral client built from the DB row. (Spec: Design > Backend, Alternatives considered.)
- The check is read-only: never persists anything, never mutates `VendorView.active`/`error`/`restart_required`. (Spec: Goals, Design > Backend.)
- The "Test connection" button always checks the *saved* vendor config, and is disabled while the page has unsaved edits. (Spec: Non-goals, Design > Frontend.)
- Saving a velos vendor auto-fires the same check immediately after a successful save, in addition to the manual button. (Spec: Design > Frontend.)
- Full gate before done: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --workspace --all-features`, `cargo deny check advisories bans licenses sources --all-features`, `clients/web` `bun run build`, and a clean `git diff --exit-code clients/ts/src/generated` after TS regen. (Spec: Testing summary; repo `Makefile`/`.github/workflows/ci.yml`.)
- No `Co-Authored-By`/AI attribution in commits (user's global convention).

Working directory for every step below: `/Users/xiaoguang/works/repos/bloomstack/october/horsie-velos-test` (git worktree, branch `velos-vendor-test`, based on `origin/main` @ `d4d1530`).

---

### Task 1: `VelosClient::whoami()`

**Files:**
- Modify: `server/src/velos/client.rs`

**Interfaces:**
- Consumes: `VelosClient` (existing, fields `http: reqwest::Client`, `base_url: String`, `token: Option<Secret>`), `VelosClient::auth()`/`VelosClient::url()` (existing private helpers), `VelosError` (existing enum: `Request(String)`, `Status { status: u16, body: String }`), `request_err(reqwest::Error) -> VelosError` (existing free fn).
- Produces: `VelosClient::whoami(&self) -> Result<String, VelosError>` — `Ok("admin")` or `Ok("worker:<name>")` on success, `Err` on a non-2xx status or transport failure. Used by Task 3.

- [ ] **Step 1: Add the `Duration` import**

In `server/src/velos/client.rs`, the top of the file currently reads:

```rust
use async_trait::async_trait;
use horsie_agentcore::Secret;
use std::collections::BTreeMap;
```

Change to:

```rust
use async_trait::async_trait;
use horsie_agentcore::Secret;
use std::collections::BTreeMap;
use std::time::Duration;
```

- [ ] **Step 2: Write the failing tests**

Find the `impl VelosClient` block (around line 90-116, ending with the `fn url` method). Leave it as-is for now — tests first.

In the `#[cfg(test)] mod tests` block at the bottom of the file, find `spawn_mock()` (around line 267-283):

```rust
    async fn spawn_mock() -> (String, MockState) {
        let st = MockState::default();
        *st.create_status.lock().unwrap() = 201;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/containers", post(mock_create))
            .route(
                "/api/v1/containers/:name",
                get(mock_get).delete(mock_delete),
            )
            .with_state(st.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), st)
    }
```

Replace it, and the `MockState` struct above it, to add a `/auth/v1/me` mock route. The full replacement for both:

```rust
    #[derive(Clone, Default)]
    struct MockState {
        posts: Arc<Mutex<Vec<serde_json::Value>>>,
        auths: Arc<Mutex<Vec<String>>>,
        /// Phase the GET handler reports; `None` → 404.
        phase: Arc<Mutex<Option<String>>>,
        /// Status code the POST handler returns (default 201).
        create_status: Arc<Mutex<u16>>,
        /// Status code the `/auth/v1/me` handler returns (default 200).
        whoami_status: Arc<Mutex<u16>>,
        /// Body the `/auth/v1/me` handler returns (default `{"identity": "admin"}`).
        whoami_body: Arc<Mutex<serde_json::Value>>,
    }
```

```rust
    async fn mock_whoami(State(st): State<MockState>) -> impl IntoResponse {
        let status = *st.whoami_status.lock().unwrap();
        let body = st.whoami_body.lock().unwrap().clone();
        (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Json(body),
        )
    }

    async fn spawn_mock() -> (String, MockState) {
        let st = MockState::default();
        *st.create_status.lock().unwrap() = 201;
        *st.whoami_status.lock().unwrap() = 200;
        *st.whoami_body.lock().unwrap() = serde_json::json!({ "identity": "admin" });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/containers", post(mock_create))
            .route(
                "/api/v1/containers/:name",
                get(mock_get).delete(mock_delete),
            )
            .route("/auth/v1/me", get(mock_whoami))
            .with_state(st.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), st)
    }
```

Then add four new tests at the end of the `mod tests` block (after the existing `phase_parse_unknown_is_not_dead` test):

```rust
    #[tokio::test]
    async fn whoami_returns_admin_identity() {
        let (base, _st) = spawn_mock().await;
        let client = VelosClient::new(base, Some(Secret::from("tok"))).unwrap();
        assert_eq!(client.whoami().await.unwrap(), "admin");
    }

    #[tokio::test]
    async fn whoami_returns_worker_identity() {
        let (base, st) = spawn_mock().await;
        *st.whoami_body.lock().unwrap() = serde_json::json!({ "identity": { "worker": "w1" } });
        let client = VelosClient::new(base, None).unwrap();
        assert_eq!(client.whoami().await.unwrap(), "worker:w1");
    }

    #[tokio::test]
    async fn whoami_maps_401_to_status_error() {
        let (base, st) = spawn_mock().await;
        *st.whoami_status.lock().unwrap() = 401;
        let client = VelosClient::new(base, Some(Secret::from("bad"))).unwrap();
        let err = client.whoami().await.unwrap_err();
        match err {
            VelosError::Status { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn whoami_reports_unreachable_server_as_request_error() {
        // Bind then drop: frees a local port nothing listens on, so the
        // connect fails fast (refused) instead of hanging.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);
        let client = VelosClient::new(format!("http://{dead_addr}"), None).unwrap();
        let err = client.whoami().await.unwrap_err();
        match err {
            VelosError::Request(_) => {}
            other => panic!("expected Request error, got {other:?}"),
        }
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie-server --lib velos::client -- whoami`
Expected: compile error — `no method named 'whoami' found for struct 'VelosClient'`.

- [ ] **Step 4: Implement `whoami()`**

In the `impl VelosClient` block, right after the existing `fn url` method (just before the block's closing `}`, around line 113-116):

```rust
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// `GET /auth/v1/me` — validate the client's token and return the
    /// authenticated identity as a display string (`"admin"` or
    /// `"worker:<name>"`). A lightweight reachability + auth check,
    /// independent of any container operation; bounded by its own timeout
    /// since (unlike the container methods) it can be triggered against a
    /// dead `server_url` by an operator clicking a button.
    pub async fn whoami(&self) -> Result<String, VelosError> {
        let resp = self
            .auth(self.http.get(self.url("/auth/v1/me")))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(request_err)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(VelosError::Status { status, body });
        }
        let doc: serde_json::Value = resp.json().await.map_err(request_err)?;
        Ok(match doc.get("identity") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Object(o)) => format!(
                "worker:{}",
                o.get("worker")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            ),
            _ => "unknown".to_string(),
        })
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server --lib velos::client`
Expected: all tests in the module pass, including the four new ones.

- [ ] **Step 6: Commit**

```bash
git add server/src/velos/client.rs
git commit -m "velos: add VelosClient::whoami() for connection checks"
```

---

### Task 2: `VendorTestResult` wire type + TS regen

**Files:**
- Modify: `models/fluorite/settings.fl`
- Regenerate (commit as generated): `clients/ts/src/generated/**`, `clients/web/src/generated/**`

**Interfaces:**
- Produces: `horsie_models::settings::VendorTestResult { ok: bool, identity: Option<String>, error: Option<String> }` (Rust, auto-generated by `models/build.rs` on `cargo build`) and the matching TS type `VendorTestResult` in both clients' `src/generated`. Used by Task 3 (Rust) and Task 5 (TS).

- [ ] **Step 1: Add the struct to the schema**

In `models/fluorite/settings.fl`, the file ends with the `SettingsUpdate` struct. Append after it:

```
/// The outcome of an on-demand connection check for a configurable vendor
/// (currently velos only) — reachability + token validity, checked live.
/// Never persisted; a fresh check every call.
struct VendorTestResult {
    ok: bool,
    /// The authenticated identity when `ok` (e.g. "admin", "worker:name").
    identity: Option<String>,
    error: Option<String>,
}
```

- [ ] **Step 2: Verify the Rust side regenerates**

Run: `cargo build -p horsie-models`
Expected: builds cleanly (the `models/build.rs` `build.rs` script recompiles all schemas in `models/fluorite/` on every build via `cargo:rerun-if-changed=fluorite`; a schema syntax error would fail this step).

- [ ] **Step 3: Regenerate the TypeScript types**

Run:
```bash
cd clients/ts && npm run generate-types && npm run typecheck && cd ../..
cd clients/web && bun run generate-types && cd ../..
```
Expected: both complete without error; `git status` shows changes under `clients/ts/src/generated/` and `clients/web/src/generated/` including a new `VendorTestResult` type.

- [ ] **Step 4: Commit**

```bash
git add models/fluorite/settings.fl clients/ts/src/generated clients/web/src/generated
git commit -m "settings: add VendorTestResult wire type"
```

---

### Task 3: `ConfigStore::test_vendor`

**Files:**
- Modify: `server/src/config/mod.rs`
- Modify: `server/src/config/store.rs`

**Interfaces:**
- Consumes: `VendorTestResult` (Task 2), `VelosClient::whoami()` (Task 1), existing `read_vendors(&self.pool)`, `resolve_velos_token(&VelosConfig)`, `VelosConfig` (existing `#[derive(Deserialize)]` struct), `VelosError` (from `crate::velos`).
- Produces: `ConfigStore::test_vendor(&self, name: &str) -> Result<VendorTestResult, String>` (trait method) implemented on `DbConfigStore`. Used by Task 4.

- [ ] **Step 1: Add the trait method**

In `server/src/config/mod.rs`, the file currently reads:

```rust
mod store;

use async_trait::async_trait;
use horsie_models::settings::{SettingsUpdate, SettingsView};

pub use store::{DbConfigStore, OpenedConfig, StoreDeps};

/// Read + mutate the runtime-editable configuration, redacting secrets.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// A redacted snapshot of the current settings, or an error if the backing
    /// store can't be read.
    async fn view(&self) -> Result<SettingsView, String>;

    /// Validate, persist, and live-apply an update. Returns the new view, or a
    /// human-readable error when the update is rejected (nothing is persisted
    /// or applied on error).
    async fn update(&self, update: SettingsUpdate) -> Result<SettingsView, String>;

    /// The vendor a create request defaults to when it omits one. Read on the
    /// hot path, so it stays synchronous and cheap.
    fn default_vendor(&self) -> String;
}
```

Change the `use` line and add the new method:

```rust
mod store;

use async_trait::async_trait;
use horsie_models::settings::{SettingsUpdate, SettingsView, VendorTestResult};

pub use store::{DbConfigStore, OpenedConfig, StoreDeps};

/// Read + mutate the runtime-editable configuration, redacting secrets.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// A redacted snapshot of the current settings, or an error if the backing
    /// store can't be read.
    async fn view(&self) -> Result<SettingsView, String>;

    /// Validate, persist, and live-apply an update. Returns the new view, or a
    /// human-readable error when the update is rejected (nothing is persisted
    /// or applied on error).
    async fn update(&self, update: SettingsUpdate) -> Result<SettingsView, String>;

    /// The vendor a create request defaults to when it omits one. Read on the
    /// hot path, so it stays synchronous and cheap.
    fn default_vendor(&self) -> String;

    /// An on-demand connection check for a configurable vendor (currently
    /// velos only): is it reachable, and does its stored token still work.
    /// Read-only — never mutates `active`/`error`/persisted state. Errs only
    /// when `name` doesn't refer to a testable vendor.
    async fn test_vendor(&self, name: &str) -> Result<VendorTestResult, String>;
}
```

- [ ] **Step 2: Write the failing tests**

In `server/src/config/store.rs`, find the `mod tests` block. It already has a `velos_input()` helper (around line 1067) and imports `use horsie_models::settings::{ModelInput, ProviderInput, VelosInput, VendorInput};` (around line 945). Add a small local mock server helper and three tests. Insert this near the end of the `mod tests` block (after the last existing test):

```rust
    // A tiny mock velos server exposing just `/auth/v1/me`, for `test_vendor`.
    async fn spawn_mock_velos(accept_token: &str) -> String {
        use axum::extract::State as AxumState;
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::get;

        #[derive(Clone)]
        struct S {
            accept: std::sync::Arc<String>,
        }
        async fn whoami(AxumState(s): AxumState<S>, headers: HeaderMap) -> impl IntoResponse {
            let ok = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("Bearer {}", s.accept))
                .unwrap_or(false);
            if ok {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({ "identity": "admin" })),
                )
            } else {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({ "error": "unauthorized" })),
                )
            }
        }
        let state = S {
            accept: std::sync::Arc::new(accept_token.to_string()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/auth/v1/me", get(whoami))
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_vendor_reports_ok_for_a_good_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let base = spawn_mock_velos("good-token").await;
        let mut input = velos_input("img", "127.0.0.1:0", None, Some("good-token"));
        if let VendorConfigInput::Velos(v) = &mut input.config {
            v.server_url = base;
        }
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![input]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        let result = o.store.test_vendor("cluster-a").await.expect("test ran");
        assert!(result.ok);
        assert_eq!(result.identity.as_deref(), Some("admin"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_vendor_reports_error_for_a_bad_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let base = spawn_mock_velos("good-token").await;
        let mut input = velos_input("img", "127.0.0.1:0", None, Some("wrong-token"));
        if let VendorConfigInput::Velos(v) = &mut input.config {
            v.server_url = base;
        }
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![input]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        let result = o.store.test_vendor("cluster-a").await.expect("test ran");
        assert!(!result.ok);
        assert!(result.identity.is_none());
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_vendor_errors_for_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let err = o.store.test_vendor("ghost").await.unwrap_err();
        assert!(err.contains("ghost"), "error names the vendor: {err}");
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie-server --lib config::store::tests::test_vendor`
Expected: compile error — `no method named 'test_vendor' found for struct 'Arc<DbConfigStore>'` (or similar — `ConfigStore` has no such method yet).

- [ ] **Step 4: Implement `test_vendor`**

In `server/src/config/store.rs`, change the import line (around line 15-18):

```rust
use crate::velos::VelosClient;
use crate::vendor::{
    LocalProcessVendor, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
};
```

to:

```rust
use crate::velos::{VelosClient, VelosError};
use crate::vendor::{
    LocalProcessVendor, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
};
```

Change the `horsie_models::settings` import (around line 22-25):

```rust
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VelosView,
    VendorConfigInput, VendorConfigView, VendorView,
};
```

to:

```rust
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VelosView,
    VendorConfigInput, VendorConfigView, VendorTestResult, VendorView,
};
```

Then, in the `impl ConfigStore for DbConfigStore` block, add the method after `fn default_vendor` (which currently ends the impl block around line 483-489):

```rust
    fn default_vendor(&self) -> String {
        self.default_vendor
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    async fn test_vendor(&self, name: &str) -> Result<VendorTestResult, String> {
        let rows = read_vendors(&self.pool).await.map_err(|e| e.to_string())?;
        let row = rows
            .into_iter()
            .find(|r| r.name == name)
            .ok_or_else(|| format!("unknown vendor '{name}'"))?;
        match row.kind.as_str() {
            "velos" => {
                let vc = serde_json::from_str::<VelosConfig>(&row.config)
                    .map_err(|e| format!("invalid config: {e}"))?;
                let token = resolve_velos_token(&vc)?;
                let client = VelosClient::new(&vc.server_url, token)
                    .map_err(|e| format!("velos client: {e}"))?;
                Ok(match client.whoami().await {
                    Ok(identity) => VendorTestResult {
                        ok: true,
                        identity: Some(identity),
                        error: None,
                    },
                    Err(VelosError::Status { status: 401, .. }) => VendorTestResult {
                        ok: false,
                        identity: None,
                        error: Some("token rejected (401 Unauthorized)".into()),
                    },
                    Err(e) => VendorTestResult {
                        ok: false,
                        identity: None,
                        error: Some(e.to_string()),
                    },
                })
            }
            other => Err(format!("vendor kind '{other}' does not support testing")),
        }
    }
}
```

(The trailing `}` above closes the `impl ConfigStore for DbConfigStore` block — replace the old closing `}` that followed `fn default_vendor`, don't duplicate it.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server --lib config::store::tests`
Expected: all tests in the module pass, including the three new ones.

- [ ] **Step 6: Commit**

```bash
git add server/src/config/mod.rs server/src/config/store.rs
git commit -m "config: add ConfigStore::test_vendor connection check"
```

---

### Task 4: HTTP route

**Files:**
- Modify: `server/src/http/config.rs`
- Modify: `server/src/http/mod.rs`

**Interfaces:**
- Consumes: `ConfigStore::test_vendor` (Task 3), `Api::internal` (existing, `server/src/http/error.rs`).
- Produces: `POST /api/config/vendors/:name/test` → `200 Json<VendorTestResult>`. Used by Task 5 (frontend client) and manual/automated smoke testing.

- [ ] **Step 1: Add the handler**

In `server/src/http/config.rs`, the file currently reads:

```rust
//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::State;
use horsie_models::settings::{SettingsUpdate, SettingsView};

/// `GET /api/config` — the current redacted settings view.
pub async fn get_config(State(state): State<AppState>) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .view()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// `PUT /api/config` — validate, persist, and live-apply an update. A rejected
/// update changes nothing and comes back as a 422 with the reason.
pub async fn update_config(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .update(update)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}
```

Replace it with:

```rust
//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use horsie_models::settings::{SettingsUpdate, SettingsView, VendorTestResult};

/// `GET /api/config` — the current redacted settings view.
pub async fn get_config(State(state): State<AppState>) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .view()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// `PUT /api/config` — validate, persist, and live-apply an update. A rejected
/// update changes nothing and comes back as a 422 with the reason.
pub async fn update_config(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .update(update)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// `POST /api/config/vendors/:name/test` — an on-demand reachability +
/// token check for a configured vendor (currently velos only). Always `200`
/// for a completed check; `ok: false` + `error` on a failed check, not an
/// HTTP error. An unknown vendor name is a 500 (mirrors `mcp::test`).
pub async fn test_vendor(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<VendorTestResult>, Api> {
    state
        .config_store
        .test_vendor(&name)
        .await
        .map(Json)
        .map_err(Api::internal)
}
```

- [ ] **Step 2: Wire the route**

In `server/src/http/mod.rs`, find:

```rust
        .route(
            "/api/config",
            get(config::get_config).put(config::update_config),
        )
```

Change to:

```rust
        .route(
            "/api/config",
            get(config::get_config).put(config::update_config),
        )
        .route(
            "/api/config/vendors/:name/test",
            post(config::test_vendor),
        )
```

- [ ] **Step 3: Write the failing test**

In `server/src/http/mod.rs`'s `mod tests` block, find `config_get_and_put_round_trip` (around line 346-377). Add a new test right after it:

```rust
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
                        "advertiseHost": "10.0.0.5",
                        "token": "tok",
                        "listen": "127.0.0.1:0"
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
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p horsie-server --lib http::vendor_test_endpoint_round_trips`
Expected: compile error or 404 — the route doesn't exist without Step 2 done first. (If you did Steps 1-2 already, this instead validates the full flow — run it now regardless to confirm.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p horsie-server --lib http::vendor_test_endpoint_round_trips`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/src/http/config.rs server/src/http/mod.rs
git commit -m "http: add POST /api/config/vendors/:name/test"
```

---

### Task 5: Frontend API client method

**Files:**
- Modify: `clients/web/src/api/client.ts`

**Interfaces:**
- Consumes: `VendorTestResult` (TS type, Task 2), `request<T>()` (existing helper in this file).
- Produces: `api.config.testVendor(name: string): Promise<VendorTestResult>`. Used by Task 6.

- [ ] **Step 1: Add the type import**

In `clients/web/src/api/client.ts`, the `import type {...} from "./types"` block includes `SettingsUpdate, SettingsView,`. Add `VendorTestResult` alphabetically:

```ts
  SessionAck,
  SettingsUpdate,
  SettingsView,
  VendorTestResult,
} from "./types";
```

- [ ] **Step 2: Add the client method**

Find the `config` object:

```ts
  config: {
    /** The current redacted settings (providers, models, vendors, deployment info). */
    get: (): Promise<SettingsView> => request("/config"),

    /** Persist + live-apply a settings update; returns the new view. */
    update: (body: SettingsUpdate): Promise<SettingsView> =>
      request("/config", { method: "PUT", body: JSON.stringify(body) }),
  },
```

Change to:

```ts
  config: {
    /** The current redacted settings (providers, models, vendors, deployment info). */
    get: (): Promise<SettingsView> => request("/config"),

    /** Persist + live-apply a settings update; returns the new view. */
    update: (body: SettingsUpdate): Promise<SettingsView> =>
      request("/config", { method: "PUT", body: JSON.stringify(body) }),

    /** On-demand reachability + token check for a vendor (velos only); never mutates settings. */
    testVendor: (name: string): Promise<VendorTestResult> =>
      request(`/config/vendors/${encodeURIComponent(name)}/test`, {
        method: "POST",
        body: "{}",
      }),
  },
```

- [ ] **Step 3: Typecheck**

Run: `cd clients/web && bun run generate-types && bun x tsc --noEmit && cd ../..`
Expected: no type errors (this also re-confirms `VendorTestResult` is present in the generated types from Task 2).

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/api/client.ts
git commit -m "web: add api.config.testVendor"
```

---

### Task 6: `useTestVendor` hook

**Files:**
- Modify: `clients/web/src/hooks/useSettings.ts`

**Interfaces:**
- Consumes: `api.config.testVendor` (Task 5).
- Produces: `useTestVendor()` — a TanStack Query mutation hook whose `mutateAsync(name: string)` resolves to `VendorTestResult`. Used by Task 7.

- [ ] **Step 1: Add the hook**

`clients/web/src/hooks/useSettings.ts` currently reads:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { SettingsUpdate, SettingsView } from "../api/types";

export const settingsKey = ["settings"] as const;

/** The server's runtime-editable configuration (providers, models, vendors). */
export function useSettings() {
  return useQuery({ queryKey: settingsKey, queryFn: () => api.config.get() });
}

/** Persist + live-apply a settings update, seeding the cache with the result. */
export function useUpdateSettings() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: SettingsUpdate) => api.config.update(body),
    onSuccess: (view: SettingsView) => client.setQueryData(settingsKey, view),
  });
}
```

Change to:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { SettingsUpdate, SettingsView } from "../api/types";

export const settingsKey = ["settings"] as const;

/** The server's runtime-editable configuration (providers, models, vendors). */
export function useSettings() {
  return useQuery({ queryKey: settingsKey, queryFn: () => api.config.get() });
}

/** Persist + live-apply a settings update, seeding the cache with the result. */
export function useUpdateSettings() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: SettingsUpdate) => api.config.update(body),
    onSuccess: (view: SettingsView) => client.setQueryData(settingsKey, view),
  });
}

/**
 * On-demand connection check for a configured vendor (velos only) — checks
 * the *saved* config, never mutates settings. Callers manage their own
 * per-vendor pending/result display since multiple checks can run at once
 * (e.g. one per vendor right after a save).
 */
export function useTestVendor() {
  return useMutation({
    mutationFn: (name: string) => api.config.testVendor(name),
  });
}
```

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun x tsc --noEmit && cd ../..`
Expected: no type errors.

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/hooks/useSettings.ts
git commit -m "web: add useTestVendor hook"
```

---

### Task 7: Settings UI — test button + auto-test after save

**Files:**
- Modify: `clients/web/src/pages/SettingsPage.tsx`

**Interfaces:**
- Consumes: `useTestVendor` (Task 6), `VendorTestResult` (TS type, Task 2), existing `VelosDraft`, `VelosRow`, `SettingsPage` state (`velos`, `dirty`, `save`).
- Produces: updated `VelosRow` (new props `testDisabled`, `test`, `onTest`) and `SettingsPage` (new state `velosTests`, new fn `runVelosTest`, `save()` fires it on success). No other file depends on this.

- [ ] **Step 1: Import the new type and hook**

In `clients/web/src/pages/SettingsPage.tsx`, the imports currently include:

```tsx
import type {
  McpServerInput,
  McpServerView,
  ModelInput,
  ProviderInput,
  SettingsView,
  VendorInput,
} from "../api/types";
```
and
```tsx
import { useSettings, useUpdateSettings } from "../hooks/useSettings";
```

Change to:

```tsx
import type {
  McpServerInput,
  McpServerView,
  ModelInput,
  ProviderInput,
  SettingsView,
  VendorInput,
  VendorTestResult,
} from "../api/types";
```
and
```tsx
import { useSettings, useTestVendor, useUpdateSettings } from "../hooks/useSettings";
```

- [ ] **Step 2: Add per-vendor test state + a runner function to `SettingsPage`**

Find the top of `SettingsPage`:

```tsx
export function SettingsPage() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [velos, setVelos] = useState<VelosDraft[]>([]);
  const [defaultVendor, setDefaultVendor] = useState("");
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
```

Change to:

```tsx
export function SettingsPage() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();
  const testVendor = useTestVendor();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [velos, setVelos] = useState<VelosDraft[]>([]);
  const [defaultVendor, setDefaultVendor] = useState("");
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [velosTests, setVelosTests] = useState<
    Record<string, { pending: boolean; result: VendorTestResult | null }>
  >({});

  const runVelosTest = async (name: string) => {
    setVelosTests((m) => ({
      ...m,
      [name]: { pending: true, result: m[name]?.result ?? null },
    }));
    try {
      const result = await testVendor.mutateAsync(name);
      setVelosTests((m) => ({ ...m, [name]: { pending: false, result } }));
    } catch (e) {
      setVelosTests((m) => ({
        ...m,
        [name]: {
          pending: false,
          result: {
            ok: false,
            identity: undefined,
            error: e instanceof ApiRequestError ? e.message : "Test failed.",
          },
        },
      }));
    }
  };
```

- [ ] **Step 3: Fire the check for every velos vendor after a successful save**

Find `save`'s final statement:

```tsx
    update.mutate({
      providers: providerInputs,
      models: modelInputs,
      vendors: vendorInputs,
      defaultVendor: defaultVendor || undefined,
    });
  };
```

Change to:

```tsx
    update.mutate(
      {
        providers: providerInputs,
        models: modelInputs,
        vendors: vendorInputs,
        defaultVendor: defaultVendor || undefined,
      },
      {
        onSuccess: (view) => {
          for (const vd of view.vendors) {
            if (vd.config?.kind === "Velos") runVelosTest(vd.name);
          }
        },
      },
    );
  };
```

- [ ] **Step 4: Pass the new props to each `VelosRow`**

Find the velos `Section`'s row loop:

```tsx
                {velos.map((v, i) => (
                  <VelosRow
                    key={i}
                    draft={v}
                    onChange={(next) => {
                      setVelos((vs) => vs.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setVelos((vs) => vs.filter((_, j) => j !== i));
                      touch();
                    }}
                  />
                ))}
```

Change to:

```tsx
                {velos.map((v, i) => (
                  <VelosRow
                    key={i}
                    draft={v}
                    onChange={(next) => {
                      setVelos((vs) => vs.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setVelos((vs) => vs.filter((_, j) => j !== i));
                      touch();
                    }}
                    testDisabled={dirty}
                    test={velosTests[v.name]}
                    onTest={() => runVelosTest(v.name)}
                  />
                ))}
```

- [ ] **Step 5: Update `VelosRow` to render the button + result**

Find the `VelosRow` function signature and its final block:

```tsx
function VelosRow({
  draft,
  onChange,
  onRemove,
}: {
  draft: VelosDraft;
  onChange: (next: VelosDraft) => void;
  onRemove: () => void;
}) {
```

Change to:

```tsx
function VelosRow({
  draft,
  onChange,
  onRemove,
  testDisabled,
  test,
  onTest,
}: {
  draft: VelosDraft;
  onChange: (next: VelosDraft) => void;
  onRemove: () => void;
  testDisabled: boolean;
  test: { pending: boolean; result: VendorTestResult | null } | undefined;
  onTest: () => void;
}) {
```

Then find the row's trailing block:

```tsx
        {draft.error && <p className="text-[11px] text-error">{draft.error}</p>}
        {!draft.active && !draft.error && draft.name.trim() && (
          <p className="text-[11px] text-faint">Not loaded yet.</p>
        )}
      </div>
    </RowShell>
  );
}
```

Change to:

```tsx
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="btn-outline text-xs"
            disabled={testDisabled || test?.pending}
            title={testDisabled ? "Save changes to test" : undefined}
            onClick={onTest}
          >
            {test?.pending && <Loader2 size={13} className="animate-spin" />}
            Test connection
          </button>
          {test?.result &&
            (test.result.ok ? (
              <span className="chip !py-0 text-[10px] text-success">
                Connected as {test.result.identity}
              </span>
            ) : (
              <span
                className="truncate text-[11px] text-error"
                title={test.result.error ?? undefined}
              >
                {test.result.error}
              </span>
            ))}
        </div>
        {draft.error && <p className="text-[11px] text-error">{draft.error}</p>}
        {!draft.active && !draft.error && draft.name.trim() && (
          <p className="text-[11px] text-faint">Not loaded yet.</p>
        )}
      </div>
    </RowShell>
  );
}
```

- [ ] **Step 6: Typecheck + build**

Run: `cd clients/web && bun x tsc --noEmit && cd ../..`
Expected: no type errors.

- [ ] **Step 7: Commit**

```bash
git add clients/web/src/pages/SettingsPage.tsx
git commit -m "web: add velos vendor Test connection button"
```

---

### Task 8: Full gate + PR

**Files:** none (verification + PR only)

**Interfaces:** none — this task only runs commands.

- [ ] **Step 1: Rust gate**

Run, in order, from the repo root:
```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```
Expected: `fmt --check` exits 0 (nothing to reformat — if `fmt --all` changed anything, `git add -u` and fold into the relevant task's commit via `git commit --amend` only if that commit hasn't been pushed, otherwise a small follow-up `fmt` commit), clippy exits 0 with no warnings, all tests pass.

- [ ] **Step 2: cargo-deny**

Run: `cargo deny check advisories bans licenses sources --all-features`
Expected: exits 0. (This change adds no new dependencies, so this should be unaffected — if it flags anything, it's pre-existing and not this change's concern.)

- [ ] **Step 3: Web build + TS drift check**

Run:
```bash
cd clients/web && bun run generate-types && bun run build && cd ../..
git diff --exit-code clients/ts/src/generated
```
Expected: `bun run build` succeeds; `git diff --exit-code` exits 0 (no drift — Task 2 already committed the regenerated files, so a fresh regen here should match byte-for-byte).

- [ ] **Step 4: Manual smoke test**

Run `horsie serve` against the temp DB with the web UI, and drive it through the browser (or `curl`) to confirm the end-to-end flow:
```bash
cargo build -p cli
./target/debug/horsie serve --addr 127.0.0.1:3789 --web clients/web/dist &
curl -s -X PUT http://127.0.0.1:3789/api/config -H 'content-type: application/json' -d '{
  "vendors": [{"name": "cluster-a", "config": {"kind": "Velos", "value": {
    "serverUrl": "http://127.0.0.1:1", "image": "img", "advertiseHost": "10.0.0.5",
    "token": "tok", "listen": "127.0.0.1:0"
  }}}]
}' | head -c 400
curl -s -X POST http://127.0.0.1:3789/api/config/vendors/cluster-a/test
```
Expected: the last `curl` prints `{"ok":false,"identity":null,"error":"..."}` (nothing real listens on `127.0.0.1:1`, so the check fails cleanly). If a real velos instance is reachable, repeat against its actual `server_url` + a valid token and confirm `{"ok":true,"identity":"admin"}` (or `"worker:..."`). Stop the server (`kill %1`) when done.

- [ ] **Step 5: Open the PR**

```bash
git push -u origin velos-vendor-test
gh pr create --title "Add a velos vendor connection test" --body "$(cat <<'EOF'
## Summary
- Add `VelosClient::whoami()` (GET /auth/v1/me — no velos changes needed, it already exists and `velosctl login` already relies on it for the same purpose).
- Add `ConfigStore::test_vendor` + `POST /api/config/vendors/:name/test`: an on-demand, read-only reachability + token check for a configured velos vendor.
- Settings UI: a "Test connection" button per velos row (disabled while there are unsaved edits) plus an automatic check right after a successful save.

## Test plan
- [x] `cargo test --workspace --all-features`
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [x] `cargo deny check`
- [x] `clients/web` `bun run build`, `git diff --exit-code clients/ts/src/generated`
- [x] Manual: PUT a velos vendor + POST its `/test` endpoint against both a dead and a live target
EOF
)"
```
Expected: PR created; note the URL for the final report.

- [ ] **Step 6: Report**

Summarize for the user: PR URL, what changed, and confirmation the full gate is green.
