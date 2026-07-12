# GitHub Connection (Server) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A deployment-global "Connect GitHub" flow (GitHub App + OAuth) whose scoped, short-lived installation tokens let repo sessions clone private repositories.

**Architecture:** GitHub App config and OAuth credentials live in the existing SQLite settings DB (migration `0002_github.sql`, single-row tables). A `GithubService` (new `server/src/github/` module) owns the pool-backed store, a GitHub API client (JWT → installation token, repo/branch listing), and a 5-minute in-memory repo cache. New `/api/github/*` endpoints drive connect/disconnect/config/repos. At `ensure_runtime` (create **and** attach), the session actor asks an injected token minter for a token scoped to exactly the session's github.com repos and injects it as the `GITHUB_TOKEN` env var — the token is never persisted and never sent to the browser.

**Tech Stack:** sqlx/SQLite (existing), reqwest (existing), `jsonwebtoken` (new, RS256 App JWT), `base64` (added in Plan 1), axum, fluorite wire types.

**Prerequisite:** Plan 1 (`2026-07-11-provision-pipeline.md`) is merged: `SessionSpec.provision`, `RuntimeSpec.env`, `repos` create flow all exist.

**Reference spec:** `docs/superpowers/specs/2026-07-11-github-repos-design.md`. Reference implementation to adapt: agentx `server/src/github_routes.rs` (at `/Users/xiaoguang/works/repos/bloomstack/agentx`).

## Global Constraints

- Wire types (API request/response) are fluorite schemas; DB row types are hand-written in the store module (never fluorite — CLAUDE.md).
- Secrets (`client_secret`, `private_key`, tokens) are `horsie_agentcore::Secret` in memory; API views expose only `has_*` booleans; inputs are write-only (omit = keep, `""` = clear) — mirror `resolve_secret` in `server/src/config/store.rs:581`.
- Production code denies `unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm`; tests opt out with the standard allow block.
- Pre-PR gate: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --check && cargo test --workspace`.
- Commit messages: short subject, no AI attribution. Feature branch off `main`.

---

### Task 1: Wire types + migration + store

**Files:**
- Create: `models/fluorite/github.fl`
- Modify: `models/src/lib.rs` (module include follows the existing per-package pattern — check how `settings` is exposed and mirror it)
- Create: `server/migrations/0002_github.sql`
- Create: `server/src/github/mod.rs`, `server/src/github/store.rs`
- Modify: `server/src/lib.rs` (add `pub mod github;` — check the actual module-root file, it may be `server/src/lib.rs`)
- Modify: `server/src/config/store.rs` + `server/src/config/mod.rs` (`OpenedConfig` gains `pub pool: SqlitePool`)
- Modify: root `Cargo.toml` (`[workspace.dependencies]`: `jsonwebtoken = "9"`), `server/Cargo.toml` (`jsonwebtoken = { workspace = true }`, `base64 = { workspace = true }`)

**Interfaces:**
- Produces (wire, `horsie_models::github`):

```
struct GitHubStatus { connected: bool, login: Option<String>, app_configured: bool, repo_count: u32 }
struct GitHubAppConfigView { client_id: String, app_id: Option<u64>, app_slug: Option<String>, has_client_secret: bool, has_private_key: bool, callback_base: Option<String> }
struct GitHubAppConfigInput { client_id: String, client_secret: Option<String>, app_id: Option<u64>, private_key: Option<String>, app_slug: Option<String>, callback_base: Option<String> }
struct GitHubRepo { full_name: String, private: bool, default_branch: String }
struct GitHubRepoList { repos: Vec<GitHubRepo> }
struct GitHubBranch { name: String }
struct GitHubBranchList { branches: Vec<GitHubBranch> }
```

- Produces (store, `server/src/github/store.rs`):

```rust
pub struct GithubStore { pool: SqlitePool }
pub struct AppConfigRow { pub client_id: String, pub client_secret: Option<Secret>, pub app_id: Option<u64>, pub private_key: Option<Secret>, pub app_slug: Option<String>, pub callback_base: Option<String> }
pub struct CredentialsRow { pub login: String, pub access_token: Secret, pub refresh_token: Option<Secret>, pub expires_at: Option<String>, pub installation_id: Option<u64> }
impl GithubStore {
    pub fn new(pool: SqlitePool) -> Self;
    pub async fn app_config(&self) -> Result<Option<AppConfigRow>, String>;
    pub async fn save_app_config(&self, input: &GitHubAppConfigInput) -> Result<AppConfigRow, String>;  // write-only secret semantics
    pub async fn credentials(&self) -> Result<Option<CredentialsRow>, String>;
    pub async fn save_credentials(&self, row: &CredentialsRow) -> Result<(), String>;
    pub async fn clear_credentials(&self) -> Result<(), String>;
}
```

- [ ] **Step 1: Dependencies**

Root `Cargo.toml` `[workspace.dependencies]`: `jsonwebtoken = "9"`. `server/Cargo.toml` `[dependencies]`: `jsonwebtoken = { workspace = true }` and `base64 = { workspace = true }`.

- [ ] **Step 2: Fluorite schema**

Create `models/fluorite/github.fl` with `package github;` and the six structs from **Interfaces** (docs on each field; `GitHubStatus.repo_count` documented as "0 until the first repo listing"). Check `models/src/lib.rs` / `models/build.rs` for how packages are registered (the build script may glob `models/fluorite/*.fl` — if a package list exists, add `github`). Build: `cargo build -p horsie-models`.

- [ ] **Step 3: Migration**

Create `server/migrations/0002_github.sql`:

```sql
-- Deployment-global GitHub connection. Single-row tables (no user/tenant
-- layer): the App the deployment authenticates as, and the one connected
-- account's OAuth credentials. Secrets are stored plaintext like provider
-- api_keys (the DB file is the trust boundary); views redact them.

CREATE TABLE github_app (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    client_id     TEXT NOT NULL,
    client_secret TEXT,
    app_id        INTEGER,
    private_key   TEXT,            -- PEM, raw or base64-encoded
    app_slug      TEXT,
    callback_base TEXT             -- e.g. "https://horsie.example.com"; NULL → derive from request
);

CREATE TABLE github_credentials (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    login           TEXT NOT NULL,
    access_token    TEXT NOT NULL,
    refresh_token   TEXT,
    expires_at      TEXT,           -- RFC 3339
    installation_id INTEGER
);
```

`sqlx::migrate!` picks it up automatically (see `server/src/config/store.rs:649`).

- [ ] **Step 4: Expose the pool**

In `server/src/config/store.rs`, add `pub pool: SqlitePool` to `OpenedConfig` and set it in `open()` (`pool: pool.clone()` before moving `pool` into `Self`). Re-export nothing new; `OpenedConfig` is already public.

- [ ] **Step 5: Failing store tests**

Create `server/src/github/mod.rs`:

```rust
//! Deployment-global GitHub connection: SQLite-backed app config + OAuth
//! credentials, a GitHub API client (App JWT → scoped installation tokens,
//! repo listing), and the session-facing token minter.

mod store;

pub use store::{AppConfigRow, CredentialsRow, GithubStore};
```

Create `server/src/github/store.rs` with the row types + `GithubStore` from **Interfaces** and this test module (write tests first):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::github::GitHubAppConfigInput;

    async fn store() -> (GithubStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (GithubStore::new(pool), tmp)
    }

    fn input(secret: Option<&str>, key: Option<&str>) -> GitHubAppConfigInput {
        GitHubAppConfigInput {
            client_id: "cid".into(),
            client_secret: secret.map(str::to_string),
            app_id: Some(7),
            private_key: key.map(str::to_string),
            app_slug: Some("horsie".into()),
            callback_base: None,
        }
    }

    #[tokio::test]
    async fn app_config_round_trips_and_keeps_omitted_secrets() {
        let (s, _t) = store().await;
        assert!(s.app_config().await.unwrap().is_none());
        s.save_app_config(&input(Some("sec"), Some("PEM"))).await.unwrap();
        // Omitted secrets keep the stored values.
        let row = s.save_app_config(&input(None, None)).await.unwrap();
        assert_eq!(row.client_secret.as_ref().map(|s| s.expose().to_string()), Some("sec".into()));
        assert_eq!(row.private_key.as_ref().map(|s| s.expose().to_string()), Some("PEM".into()));
        // Empty string clears.
        let row = s.save_app_config(&input(Some(""), None)).await.unwrap();
        assert!(row.client_secret.is_none());
    }

    #[tokio::test]
    async fn credentials_save_read_clear() {
        let (s, _t) = store().await;
        assert!(s.credentials().await.unwrap().is_none());
        s.save_credentials(&CredentialsRow {
            login: "octo".into(),
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            installation_id: Some(42),
        })
        .await
        .unwrap();
        let c = s.credentials().await.unwrap().unwrap();
        assert_eq!(c.login, "octo");
        assert_eq!(c.installation_id, Some(42));
        s.clear_credentials().await.unwrap();
        assert!(s.credentials().await.unwrap().is_none());
    }
}
```

Note: `Secret` comes from `horsie_agentcore::Secret`. Check its API first (`grep -n "impl" agentcore/src/*.rs | grep -i secret` or open the file): if the accessor isn't `expose()`, use the actual method (the velos config code at `server/src/config/store.rs:502-520` shows working usage — `Secret::from(v)`, `t.is_empty()`, serde transparency). Adjust the test accordingly.

Run: `cargo test -p horsie-server github::store 2>&1 | head -5` — expect compile FAIL.

- [ ] **Step 6: Implement the store**

`GithubStore` methods use plain `sqlx::query` like `config/store.rs`. Single-row upsert pattern:

```rust
    pub async fn save_app_config(
        &self,
        input: &GitHubAppConfigInput,
    ) -> Result<AppConfigRow, String> {
        let existing = self.app_config().await?;
        let client_secret = resolve_secret(
            &input.client_secret,
            existing.as_ref().and_then(|e| e.client_secret.as_ref()),
        );
        let private_key = resolve_secret(
            &input.private_key,
            existing.as_ref().and_then(|e| e.private_key.as_ref()),
        );
        sqlx::query(
            "INSERT INTO github_app (id, client_id, client_secret, app_id, private_key, app_slug, callback_base) \
             VALUES (1, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET client_id = excluded.client_id, \
             client_secret = excluded.client_secret, app_id = excluded.app_id, \
             private_key = excluded.private_key, app_slug = excluded.app_slug, \
             callback_base = excluded.callback_base",
        )
        .bind(input.client_id.trim())
        .bind(client_secret.as_ref().map(|s| s.expose().to_string()))
        .bind(input.app_id.map(|v| v as i64))
        .bind(private_key.as_ref().map(|s| s.expose().to_string()))
        .bind(trimmed(&input.app_slug))
        .bind(trimmed(&input.callback_base))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.app_config()
            .await?
            .ok_or_else(|| "github app config missing after save".to_string())
    }
```

with local helpers mirroring `config/store.rs`:

```rust
/// Write-only secret input: `None` keeps stored, `Some("")` clears, `Some(v)` sets.
fn resolve_secret(input: &Option<String>, existing: Option<&Secret>) -> Option<Secret> {
    match input {
        None => existing.cloned(),
        Some(v) if !v.is_empty() => Some(Secret::from(v.as_str())),
        Some(_) => None,
    }
}

fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
}
```

`app_config()` / `credentials()` are `SELECT … WHERE id = 1` + `fetch_optional`; `save_credentials` is the same upsert shape; `clear_credentials` is `DELETE FROM github_credentials`. `app_id`/`installation_id` round-trip through `i64` (`try_get::<Option<i64>, _>` then `u64::try_from(..).ok()`).

Also register the module: add `pub mod github;` to the server crate root (the file that declares `pub mod config; pub mod http; …`).

- [ ] **Step 7: Run tests, commit**

Run: `cargo test -p horsie-server github`
Expected: PASS.

```bash
git add -A
git commit -m "server: github app config + credentials store"
```

---

### Task 2: GitHub API client

**Files:**
- Create: `server/src/github/api.rs`
- Modify: `server/src/github/mod.rs` (`mod api; pub use api::GithubApi;`)

**Interfaces:**
- Produces:

```rust
pub struct GithubApi { web_base: String, api_base: String, http: reqwest::Client }
impl GithubApi {
    pub fn new() -> Self;                                  // github.com defaults
    pub fn with_bases(web_base: &str, api_base: &str) -> Self;  // tests inject a mock server
    pub fn authorize_url(&self, client_id: &str, redirect_uri: &str) -> String;
    pub async fn exchange_code(&self, client_id: &str, client_secret: &str, code: &str, redirect_uri: &str) -> Result<ExchangedToken, String>;
    pub async fn user_installation_id(&self, access_token: &str, app_id: u64) -> Result<Option<u64>, String>;
    pub async fn installation_token(&self, app_id: u64, pem: &str, installation_id: u64, repos: &[String]) -> Result<String, String>;  // repos = short names; empty = unscoped
    pub async fn list_installation_repos(&self, app_id: u64, pem: &str, installation_id: u64) -> Result<Vec<GitHubRepo>, String>;      // paginated
    pub async fn list_branches(&self, token: &str, full_name: &str) -> Result<Vec<GitHubBranch>, String>;
}
pub struct ExchangedToken { pub login: String, pub access_token: String, pub refresh_token: Option<String>, pub expires_at: Option<String> }
pub fn decode_private_key(raw: &str) -> Result<String, String>;   // raw PEM or base64(PEM)
pub fn make_app_jwt(app_id: u64, pem: &str) -> Result<String, String>;
```

- [ ] **Step 1: Failing unit tests**

In `server/src/github/api.rs`, write the test module first:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 2048-bit RSA test key, generated for tests only (not a real credential).
    /// Generate once with: openssl genrsa 2048
    const TEST_PEM: &str = include_str!("testdata/test_rsa.pem");

    #[test]
    fn decode_private_key_accepts_raw_and_base64() {
        assert_eq!(decode_private_key(TEST_PEM).unwrap().trim(), TEST_PEM.trim());
        let b64 = base64::engine::general_purpose::STANDARD.encode(TEST_PEM);
        assert_eq!(decode_private_key(&b64).unwrap().trim(), TEST_PEM.trim());
        assert!(decode_private_key("garbage").is_err());
    }

    #[test]
    fn make_app_jwt_produces_rs256_token() {
        let jwt = make_app_jwt(1234, TEST_PEM).unwrap();
        // header.payload.signature
        assert_eq!(jwt.split('.').count(), 3);
        let header = jwt.split('.').next().unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .unwrap();
        assert!(String::from_utf8_lossy(&decoded).contains("RS256"));
    }

    #[test]
    fn authorize_url_carries_client_and_redirect() {
        let api = GithubApi::new();
        let url = api.authorize_url("cid-1", "https://h.example/api/github/callback");
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(url.contains("client_id=cid-1"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fh.example%2Fapi%2Fgithub%2Fcallback"));
    }

    #[tokio::test]
    async fn installation_token_scopes_to_repo_short_names() {
        // Mock GitHub: capture the token-request body, return a token.
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/app/installations/:id/access_tokens",
                post(
                    |State(cap): State<Arc<Mutex<Option<serde_json::Value>>>>,
                     Json(body): Json<serde_json::Value>| async move {
                        *cap.lock().unwrap() = Some(body);
                        Json(serde_json::json!({"token": "ghs_test"}))
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let api = GithubApi::with_bases("http://x", &format!("http://{addr}"));
        let token = api
            .installation_token(1234, TEST_PEM, 42, &["api".into(), "web".into()])
            .await
            .unwrap();
        assert_eq!(token, "ghs_test");
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["repositories"], serde_json::json!(["api", "web"]));
    }
}
```

Generate the fixture key: `mkdir -p server/src/github/testdata && openssl genrsa -out server/src/github/testdata/test_rsa.pem 2048` (commit it — it is a test-only fixture, never a real credential; note this in a comment at the top usage site as shown).

Run: `cargo test -p horsie-server github::api 2>&1 | head -5` — expect compile FAIL.

- [ ] **Step 2: Implement**

`server/src/github/api.rs` (adapted from agentx `github_routes.rs:125-260`, converted to `Result<_, String>` and base-injectable):

```rust
//! GitHub REST client: OAuth code exchange, App JWT, scoped installation
//! tokens, repo/branch listing. Bases are injectable so tests run against a
//! local mock server.

use base64::Engine;
use horsie_models::github::{GitHubBranch, GitHubRepo};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct GithubApi {
    web_base: String,
    api_base: String,
    http: reqwest::Client,
}

pub struct ExchangedToken {
    pub login: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
}

impl GithubApi {
    pub fn new() -> Self {
        Self::with_bases("https://github.com", "https://api.github.com")
    }

    pub fn with_bases(web_base: &str, api_base: &str) -> Self {
        Self {
            web_base: web_base.trim_end_matches('/').to_string(),
            api_base: api_base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("horsie")
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn authorize_url(&self, client_id: &str, redirect_uri: &str) -> String {
        format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}",
            self.web_base,
            urlencode(client_id),
            urlencode(redirect_uri),
        )
    }
    // … remaining methods below …
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
```

`exchange_code`: POST `{web_base}/login/oauth/access_token` with form/json `{client_id, client_secret, code, redirect_uri}` and header `Accept: application/json`; deserialize `{access_token, refresh_token, expires_in, error_description}`; error when `access_token` missing. Then GET `{api_base}/user` with `Authorization: Bearer <token>` to read `login`. `expires_at` = now + `expires_in` seconds formatted RFC 3339 — compute with `SystemTime` seconds only (`format!` epoch is fine for storage; keep it a plain string).

`make_app_jwt` (module-level fn): agentx's implementation verbatim, adapted to `Result<String, String>`:

```rust
/// Short-lived JWT authenticating as the GitHub App (10-minute max).
pub fn make_app_jwt(app_id: u64, pem: &str) -> Result<String, String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    #[derive(serde::Serialize)]
    struct Claims { iat: u64, exp: u64, iss: String }
    let claims = Claims {
        iat: now.saturating_sub(60), // clock-skew buffer
        exp: now + 540,              // 9 min (max 10)
        iss: app_id.to_string(),
    };
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| format!("invalid RSA private key: {e}"))?;
    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| format!("JWT encode: {e}"))
}

/// Accept a raw PEM or a base64-encoded PEM (copy-paste friendly).
pub fn decode_private_key(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.starts_with("-----BEGIN") {
        return Ok(t.to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(t)
        .map_err(|_| "private key is neither a PEM nor base64-encoded PEM".to_string())?;
    let s = String::from_utf8(bytes).map_err(|_| "decoded private key is not UTF-8".to_string())?;
    if !s.trim_start().starts_with("-----BEGIN") {
        return Err("decoded value is not a PEM".to_string());
    }
    Ok(s)
}
```

`installation_token`: build JWT, POST `{api_base}/app/installations/{id}/access_tokens` with `Accept: application/vnd.github+json`, bearer JWT, body `{}` when `repos` empty else `{"repositories": [short names]}` (callers pass short names — the service layer strips owners); deserialize `{token}`.

`user_installation_id`: GET `{api_base}/user/installations` bearer `access_token`, find the installation whose `app_id` matches, return its `id` (agentx `github_routes.rs:372`).

`list_installation_repos`: mint an unscoped installation token, then GET `{api_base}/installation/repositories?per_page=100&page=N` bearer that token, accumulating `repositories[].{full_name, private, default_branch}` until a page returns fewer than 100.

`list_branches`: GET `{api_base}/repos/{full_name}/branches?per_page=100` bearer token → `Vec<GitHubBranch>`.

- [ ] **Step 3: Run tests, commit**

Run: `cargo test -p horsie-server github`
Expected: PASS.

```bash
git add -A
git commit -m "server: github api client (jwt, tokens, repo listing)"
```

---

### Task 3: `GithubService` + HTTP endpoints

**Files:**
- Create: `server/src/github/service.rs`, `server/src/http/github.rs`
- Modify: `server/src/github/mod.rs`, `server/src/http/mod.rs` (routes + `AppState.github`)
- Modify: `cli/src/serve.rs` (construct the service from `opened.pool`)

**Interfaces:**
- Consumes: `GithubStore` (Task 1), `GithubApi` (Task 2).
- Produces:

```rust
pub struct GithubService { store: GithubStore, api: GithubApi, cache: tokio::sync::Mutex<Option<(std::time::Instant, Vec<GitHubRepo>)>> }
impl GithubService {
    pub fn new(store: GithubStore, api: GithubApi) -> Self;
    pub async fn status(&self) -> Result<GitHubStatus, String>;
    pub async fn app_config_view(&self) -> Result<Option<GitHubAppConfigView>, String>;
    pub async fn save_app_config(&self, input: GitHubAppConfigInput) -> Result<GitHubAppConfigView, String>;
    pub async fn auth_redirect(&self, request_base: &str) -> Result<String, String>;           // authorize URL
    pub async fn handle_callback(&self, code: &str, request_base: &str) -> Result<(), String>; // exchange + store + discover installation
    pub async fn disconnect(&self) -> Result<(), String>;
    pub async fn repos(&self, refresh: bool) -> Result<Vec<GitHubRepo>, String>;               // 5-min cache
    pub async fn branches(&self, full_name: &str) -> Result<Vec<GitHubBranch>, String>;
    pub async fn mint_token_for(&self, repo_urls: &[String]) -> Result<Option<String>, String>; // Task 4 uses this
}
```

- HTTP routes (all under the existing `/api` router): `GET /api/github/status`, `GET /api/github/auth`, `GET /api/github/callback?code=…`, `GET|PUT /api/github/app-config`, `DELETE /api/github/disconnect`, `GET /api/github/repos[?refresh=1]`, `GET /api/github/repos/branches?repo=owner/name`.
- `AppState` gains `pub github: Arc<GithubService>`.

- [ ] **Step 1: Failing HTTP tests**

Add to `server/src/http/mod.rs` tests (extend `test_state` to build a `GithubService` over the test pool — `crate::github::GithubService::new(crate::github::GithubStore::new(opened.pool.clone()), crate::github::GithubApi::new())`):

```rust
    #[tokio::test]
    async fn github_status_and_app_config_round_trip() {
        use horsie_models::github::{GitHubAppConfigView, GitHubStatus};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // Fresh deployment: nothing configured.
        let res = app.clone().oneshot(get("/api/github/status")).await.unwrap();
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
        let res = app.clone().oneshot(get("/api/github/status")).await.unwrap();
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
        let res = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/github/disconnect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
```

Run: `cargo test -p horsie-server http 2>&1 | head -5` — expect FAIL (no routes, no `AppState.github`).

- [ ] **Step 2: Implement `GithubService`**

`server/src/github/service.rs`. Key behaviors:

- `status()`: `connected` = credentials row exists; `login` from it; `app_configured` = app row exists with non-empty `client_id`; `repo_count` = cached repo list length (0 when cold — do not hit the network from `status`).
- `auth_redirect(request_base)`: needs app config with `client_id`; `redirect_uri` = `{callback_base or request_base}/api/github/callback`; returns `api.authorize_url(...)`.
- `handle_callback(code, request_base)`: requires app config (client_id + client_secret); `exchange_code`; then if `app_id` set, `user_installation_id`; `save_credentials`; clear the repo cache.
- `repos(refresh)`: needs app config with `app_id` + `private_key` and credentials with `installation_id` ("connect GitHub first" error otherwise). Serve from cache when fresh (< 300 s) and `!refresh`; else `decode_private_key` → `list_installation_repos` → cache + return.
- `mint_token_for(repo_urls)`: filter URLs to `https://github.com/` ones; if none → `Ok(None)`; if GitHub not connected/app incomplete → `Ok(None)` (public repos still clone tokenless); else extract short repo names (`…/owner/repo(.git)` → `repo`), dedupe, `installation_token(app_id, pem, installation_id, &names)` → `Ok(Some(token))`. Errors from the API surface as `Err` (fail visibly — a private-repo session with a broken App should not silently clone-fail later).

- [ ] **Step 3: HTTP layer**

`server/src/http/github.rs` — axum handlers mapping the service; errors → the existing `Api` envelope (`crate::http::error::Api`); `auth`/`callback` return redirects:

```rust
pub async fn auth(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Result<impl IntoResponse, Api> {
    let base = request_base(&headers);
    let url = state.github.auth_redirect(&base).await.map_err(Api::unprocessable)?;
    Ok(axum::response::Redirect::temporary(&url))
}

pub async fn callback(State(state): State<AppState>, Query(q): Query<CallbackQuery>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let base = request_base(&headers);
    let dest = match q.code {
        Some(code) => match state.github.handle_callback(&code, &base).await {
            Ok(()) => "/settings?github_connected=1".to_string(),
            Err(e) => format!("/settings?github_error={}", urlencode(&e)),
        },
        None => format!(
            "/settings?github_error={}",
            urlencode(&q.error_description.or(q.error).unwrap_or_else(|| "authorization denied".into()))
        ),
    };
    axum::response::Redirect::temporary(&dest)
}

/// "http://host" from the request headers (horsie serves same-origin; a
/// configured callback_base overrides this inside the service).
fn request_base(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

#[derive(serde::Deserialize)]
pub struct CallbackQuery { pub code: Option<String>, pub error: Option<String>, pub error_description: Option<String> }
```

(`urlencode` — reuse the one from `github::api` by making it `pub(crate)`.) Remaining handlers are thin: `status`, `get_app_config` (404-free: return `GitHubAppConfigView` with empty defaults when unset — simpler for the UI: wrap in the view with `client_id: ""`), `put_app_config`, `disconnect`, `repos` (`?refresh=1` query), `branches` (`?repo=` query; mint an unscoped installation token inside the service for the API call).

Routes in `server/src/http/mod.rs::app()`:

```rust
        .route("/api/github/status", get(github::status))
        .route("/api/github/auth", get(github::auth))
        .route("/api/github/callback", get(github::callback))
        .route(
            "/api/github/app-config",
            get(github::get_app_config).put(github::put_app_config),
        )
        .route("/api/github/disconnect", axum::routing::delete(github::disconnect))
        .route("/api/github/repos", get(github::repos))
        .route("/api/github/repos/branches", get(github::branches))
```

`AppState` gains `pub github: Arc<crate::github::GithubService>`. Update `test_state` (step 1) and `cli/src/serve.rs`:

```rust
    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    // … AppState { …, github, … }
```

- [ ] **Step 4: Run, commit**

Run: `cargo test -p horsie-server`
Expected: PASS including the new HTTP tests.

```bash
git add -A
git commit -m "server: github connect endpoints (oauth, app config, repos)"
```

---

### Task 4: Token injection at create/attach

**Files:**
- Modify: `server/src/sessions/spec.rs` (`ServerDeps.github_tokens`)
- Modify: `server/src/sessions/session_actor.rs` (`ensure_runtime` minting)
- Modify: `server/src/github/mod.rs` (minter trait + impl for `GithubService`)
- Modify: `cli/src/serve.rs`, `server/src/http/mod.rs` tests, `tests/tests/session_server_e2e.rs` harness (deps fixture)

**Interfaces:**
- Produces:

```rust
// server/src/github/mod.rs
/// Mints a short-lived GitHub token scoped to exactly `repo_urls`' repos.
/// `Ok(None)` = nothing to mint (no github URLs / not connected) — proceed
/// tokenless. `Err` = the connection is configured but minting failed.
#[async_trait::async_trait]
pub trait GithubTokenMinter: Send + Sync {
    async fn mint_for(&self, repo_urls: &[String]) -> Result<Option<String>, String>;
}
// ServerDeps gains: pub github_tokens: Option<Arc<dyn GithubTokenMinter>>
```

- [ ] **Step 1: Failing session-actor test**

In `server/src/sessions/session_actor.rs` tests, add a minter double + test (the `MockVendor::last_create_spec()` recorder from Plan 1 Task 5 exposes the spec):

```rust
    struct FixedMinter(Option<String>);
    #[async_trait]
    impl crate::github::GithubTokenMinter for FixedMinter {
        async fn mint_for(&self, _repo_urls: &[String]) -> Result<Option<String>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn provision_mints_github_token_into_env() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let vendor = MockVendor::new();
        let mut h = harness_on(journal, vendor); // then override deps: see below
        // Build a harness variant whose spec has a github provision step and
        // whose deps carry the minter (mirror harness_with_id, adding:
        //   spec.provision = vec![ProvisionStepSpec {
        //       name: "checkout api".into(),
        //       uses: "git_checkout".into(),
        //       with: vec![("url".into(), "https://github.com/o/api".into()),
        //                  ("dir".into(), "api".into())],
        //   }];
        //   deps.github_tokens = Some(Arc::new(FixedMinter(Some("ghs_x".into()))));
        // )
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
        let spec = h.vendor.last_create_spec().expect("vendor saw a spec");
        assert!(
            spec.env
                .iter()
                .any(|e| e.name == horsie_models::ENV_GITHUB_TOKEN && e.value == "ghs_x"),
            "GITHUB_TOKEN injected: {:?}",
            spec.env
        );
    }
```

(Restructure the existing `harness_with_id` to accept the spec + deps mutation closures, or add a `harness_custom(journal, vendor, id, |spec|, |deps|)` helper — follow whichever shape keeps the existing tests compiling with minimal churn.)

Run: `cargo test -p horsie-server session_actor 2>&1 | head -5` — expect compile FAIL.

- [ ] **Step 2: Implement**

`server/src/sessions/spec.rs` — `ServerDeps` gains:

```rust
    /// Mints short-lived GitHub tokens for repo provisioning; `None` when the
    /// deployment has no GitHub integration wired.
    pub github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
```

`server/src/github/mod.rs` — the trait from **Interfaces** plus:

```rust
#[async_trait::async_trait]
impl GithubTokenMinter for GithubService {
    async fn mint_for(&self, repo_urls: &[String]) -> Result<Option<String>, String> {
        self.mint_token_for(repo_urls).await
    }
}
```

`server/src/sessions/session_actor.rs` — `ensure_runtime` becomes:

```rust
    async fn ensure_runtime(&mut self, mode: WakeMode) -> Result<(), String> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let vendor = self.vendor()?;
        let mut rt_spec = self.write_runtime_spec()?;
        // Fresh, scoped token at every create AND attach — never persisted.
        if let Some(minter) = &self.deps.github_tokens {
            let urls: Vec<String> = rt_spec
                .provision
                .iter()
                .filter(|s| s.uses == "git_checkout")
                .filter_map(|s| {
                    s.with
                        .iter()
                        .find(|p| p.key == "url")
                        .map(|p| p.value.clone())
                })
                .collect();
            if !urls.is_empty()
                && let Some(token) = minter.mint_for(&urls).await?
            {
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_GITHUB_TOKEN.to_string(),
                    value: token,
                });
            }
        }
        let id = self.id.to_string();
        let runtime = match mode {
            WakeMode::Create => vendor.create(&id, &rt_spec).await,
            WakeMode::Attach => vendor.attach(&id, &rt_spec).await,
        }
        .map_err(|e| e.to_string())?;
        self.runtime = Some(runtime);
        Ok(())
    }
```

Update every `ServerDeps { … }` literal (session_actor tests, http/mod.rs tests, e2e harness, `cli/src/serve.rs`) — tests use `github_tokens: None`; `serve.rs` uses `github_tokens: Some(github.clone())` (the same `Arc<GithubService>` that went into `AppState`).

- [ ] **Step 3: Run the full suite, commit**

Run: `cargo test --workspace`
Expected: PASS.

```bash
git add -A
git commit -m "server: mint scoped github token at create/attach"
```

---

### Task 5: Full gate + docs touch-up

- [ ] **Step 1: Pre-PR gate**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --check && cargo test --workspace`
Expected: clean. Fix anything found (`cargo fmt --all`).

- [ ] **Step 2: README/docs note**

Add a short "GitHub integration" subsection to the server docs (wherever `horsie serve` is documented — check `docs/` and `README.md`): the App needs `Contents: Read` repository permission; configure client id/secret/app id/private key in Settings → GitHub, then Connect. One paragraph, no marketing.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "docs: github connection setup"
```
