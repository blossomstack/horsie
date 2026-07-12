# DB-managed skill bundles — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the horsie session server a DB-backed, web-managed library of plugin bundles (git-installed), let users select bundles per session, and have each session's runtime fetch its selected bundles by authed URL and unpack them into a plugins dir the existing scanner reads — one path for local and velos.

**Architecture:** A `PluginService` (server crate, mirrors `GithubService`) owns a `plugins` table (shared SqlitePool) + a content-addressed zip artifact store under the data dir. CRUD over `/api/plugins`; a token-guarded `/api/plugins/artifacts/{hash}.zip`. At session provisioning, `ensure_runtime` resolves selected bundle names → `{name,hash,url}`, mints an HS256 capability token, and pushes `HORSIE_PLUGIN_MANIFEST`/`HORSIE_PLUGINS_TOKEN` (+ vendor-supplied `HORSIE_PLUGINS_DIR`/`HORSIE_PLUGINS_CACHE`) into `rt_spec.env` — exactly like the existing GitHub-token block. The runtime, before announcing ready, fetches+verifies+unpacks each bundle into the plugins dir; the existing `ScanWorkspace(include_shared)` + `RunSessionStart` machinery is untouched.

**Tech Stack:** Rust (axum, sqlx/SQLite, jsonwebtoken HS256, `zip`, `sha2`, `reqwest`, `git` CLI), fluorite schema codegen (Rust + TS), Bun + Vite + React 19 + Tailwind + React Query web client.

## Global Constraints

- **Migration numbering:** next free is `0003_plugins.sql` (`0001_init.sql`, `0002_github.sql` exist on origin/main).
- **No new deps beyond `sha2` + `zip`.** Reuse `jsonwebtoken` (HS256) for the token; `reqwest` for runtime fetch; `tempfile` for clone/unpack temp dirs. New crates must be permissive-licensed (deny.toml: MIT/Apache-2.0/BSD/ISC/…); `multiple-versions = "warn"`.
- **fluorite regen:** after editing any `.fl`, regenerate. `clients/ts` `generate-types` must be re-run whenever `session_api.fl` changes (CI ts-drift checks `clients/ts` only); `clients/web` `generate-types` gains `plugins.fl`. `git diff --exit-code clients/ts/src/generated` must be clean.
- **No AI attribution** in commits/PR (user global pref). Work on branch `skills-plugins` (worktree `horsie-skills`).
- **Full gate (must be green before PR):** `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`; `cargo deny check`; `clients/ts` regen + clean drift; `cd clients/web && bun run build`.
- **Trust model:** installed bundles are trusted; hooks run on both vendors (no gating). The `/api/plugins` write routes are admin surface (same auth posture as the rest of `/api`).
- **Versioning:** session spec stores bundle **names**; hash resolves live at each provisioning (latest-at-start). Old artifacts GC'd when unreferenced.

---

## File structure

**New (server):**
- `server/migrations/0003_plugins.sql` — `plugins` table.
- `server/src/plugins/mod.rs` — re-exports; `PluginArtifactRef`, `PluginProvisioner` trait.
- `server/src/plugins/store.rs` — `PluginStore` (sqlx CRUD on the shared pool).
- `server/src/plugins/artifact.rs` — content-addressed zip store under data dir (write/read/gc).
- `server/src/plugins/ingest.rs` — git clone → locate root → inspect (skills/hooks) → deterministic zip → sha256.
- `server/src/plugins/token.rs` — HS256 capability token encode/verify.
- `server/src/plugins/service.rs` — `PluginService` (CRUD + resolve + mint_token); impls `PluginProvisioner`.
- `server/src/http/plugins.rs` — 5 CRUD routes + token-guarded artifact route.

**New (runtime):**
- `runtime/src/plugins_fetch.rs` — read manifest env → fetch/verify/unpack → return plugins dir.

**New (schema/web):**
- `models/fluorite/plugins.fl` — `PluginView`/`PluginInstallInput`/`PluginDefaultInput`.
- `clients/web/src/pages/SkillsPage.tsx`, `clients/web/src/hooks/usePlugins.ts`.

**Modified:**
- `models/src/lib.rs` — `ENV_PLUGIN_MANIFEST`/`ENV_PLUGINS_TOKEN`/`ENV_PLUGINS_DIR`/`ENV_PLUGINS_CACHE`.
- `models/fluorite/session_api.fl` — `CreateSessionRequest.plugins`.
- `server/src/lib.rs` (module decl `pub mod plugins;`), `server/src/http/mod.rs` (routes + `AppState.plugins`), `server/src/http/handlers.rs` (`create_session` maps `req.plugins`).
- `server/src/sessions/spec.rs` — `SessionSpec.plugins: Vec<String>`; `ServerDeps.plugins: Option<Arc<dyn PluginProvisioner>>`.
- `server/src/sessions/session_actor.rs` — `ensure_runtime` plugin-env block; `write_runtime_spec` unchanged.
- `server/src/vendor/mod.rs` — trait methods `artifact_base_url`/`plugins_dir_for`/`plugins_cache_dir` (default `None`); `server/src/vendor/local.rs` + `velos.rs` overrides + config.
- `server/src/config/store.rs` — velos build reads `public_http_base`/`http_port`; `models/fluorite/settings.fl` VelosView/VelosInput gain those two fields.
- `runtime/src/main.rs` — call `plugins_fetch` before ready, use returned dir as plugins_dir.
- `cli/src/serve.rs` — construct `PluginService`, inject into `AppState.plugins` + `ServerDeps.plugins`; pass server public base to `LocalProcessVendor`; config for artifact dir + token secret + public base.
- `clients/web`: `api/client.ts` (`api.plugins.*`), `App.tsx` (route), `components/Sidebar.tsx` (nav), `components/NewSessionModal.tsx` (bundle multi-select), `src/api/types.ts` (+plugins export), `package.json` generate-types (+plugins.fl).
- `server/Cargo.toml` — add `sha2`, `zip`; `runtime/Cargo.toml` — add `sha2`, `zip` (reqwest already workspace); root `Cargo.toml` workspace deps for `sha2`/`zip`.
- `docker/server.Dockerfile` (install `git`); `october/ops/horsie/{docker-compose.yml,RUNBOOK.md}` (config + seeding note).

---

## Phase 1 — Schema, deps, env constants, migration

### Task 1: Add `sha2` + `zip` deps and env constants

**Files:**
- Modify: root `Cargo.toml` (`[workspace.dependencies]`), `server/Cargo.toml`, `runtime/Cargo.toml`
- Modify: `models/src/lib.rs`

**Interfaces:**
- Produces: `horsie_models::ENV_PLUGIN_MANIFEST`, `ENV_PLUGINS_TOKEN`, `ENV_PLUGINS_DIR`, `ENV_PLUGINS_CACHE` (`&str` consts).

- [ ] **Step 1:** Add to root `Cargo.toml` `[workspace.dependencies]`: `sha2 = "0.10"` and `zip = { version = "2", default-features = false, features = ["deflate"] }`. Add `sha2` + `zip` to `server/Cargo.toml` and `runtime/Cargo.toml` `[dependencies]` as `{ workspace = true }`.
- [ ] **Step 2:** In `models/src/lib.rs`, next to the existing `ENV_PROVISION`/`ENV_GITHUB_TOKEN` consts, add:
```rust
/// JSON array of `{name, hash, url}` bundle refs the runtime should fetch.
pub const ENV_PLUGIN_MANIFEST: &str = "HORSIE_PLUGIN_MANIFEST";
/// Bearer token the runtime sends when fetching bundle artifacts.
pub const ENV_PLUGINS_TOKEN: &str = "HORSIE_PLUGINS_TOKEN";
/// Directory the runtime unpacks bundles into and scans as plugins_dir.
pub const ENV_PLUGINS_DIR: &str = "HORSIE_PLUGINS_DIR";
/// Optional content-hash cache dir (local vendor) to avoid re-fetch/unpack.
pub const ENV_PLUGINS_CACHE: &str = "HORSIE_PLUGINS_CACHE";
```
- [ ] **Step 3:** `cargo build -p horsie-models` — expect success. Then `cargo deny check` — expect no new denials (both crates are MIT/Apache-2.0).
- [ ] **Step 4:** Commit: `git commit -am "plugins: add sha2/zip deps + plugin env constants"`.

### Task 2: `0003_plugins.sql` migration

**Files:**
- Create: `server/migrations/0003_plugins.sql`
- Test: covered by Task 6 (store tests run migrations via `sqlx::migrate!`).

- [ ] **Step 1:** Write the migration:
```sql
CREATE TABLE plugins (
    name            TEXT PRIMARY KEY,
    source_kind     TEXT NOT NULL,            -- 'git'
    source_url      TEXT NOT NULL,
    source_ref      TEXT,
    version         TEXT,
    description     TEXT,
    skill_count     INTEGER NOT NULL DEFAULT 0,
    has_hooks       INTEGER NOT NULL DEFAULT 0,
    artifact_hash   TEXT NOT NULL,
    artifact_size   INTEGER NOT NULL,
    enabled_default INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
```
- [ ] **Step 2:** `cargo build -p horsie-server` (compiles `sqlx::migrate!()` which validates the migrations dir) — expect success.
- [ ] **Step 3:** Commit: `git commit -am "plugins: add 0003_plugins.sql migration"`.

---

## Phase 2 — PluginStore, artifact store, token, ingest (server core)

### Task 3: Fluorite `plugins.fl` + `CreateSessionRequest.plugins`

**Files:**
- Create: `models/fluorite/plugins.fl`
- Modify: `models/fluorite/session_api.fl`
- Modify: `clients/ts/package.json`, `clients/web/package.json` (generate-types), `clients/web/src/api/types.ts`

**Interfaces:**
- Produces (Rust, via build.rs): `horsie_models::plugins::{PluginView, PluginInstallInput, PluginDefaultInput}`; `CreateSessionRequest.plugins: Option<Vec<String>>`.

- [ ] **Step 1:** Write `models/fluorite/plugins.fl` (package `plugins`), mirroring `settings.fl` doc-comment style:
```
struct PluginView {
    name: String,
    description: Option<String>,
    version: Option<String>,
    source_url: String,
    source_ref: Option<String>,
    skill_count: u32,
    has_hooks: bool,
    enabled_default: bool,
    artifact_size: u64,
}
struct PluginInstallInput { source_url: String, source_ref: Option<String> }
struct PluginDefaultInput { enabled_default: bool }
```
- [ ] **Step 2:** In `session_api.fl`, add to `CreateSessionRequest`: `plugins: Option<Vec<String>>,` (selected bundle names; None ⇒ enabled_default set).
- [ ] **Step 3:** `cargo build -p horsie-models` — expect the generated Rust types to compile.
- [ ] **Step 4:** Add `../../models/fluorite/plugins.fl` to `clients/web/package.json` `generate-types`. Regenerate both clients: `cd clients/ts && bun run generate-types` and `cd clients/web && bun run generate-types`. Add `export * from "../generated/plugins";` to `clients/web/src/api/types.ts`.
- [ ] **Step 5:** `git diff --exit-code clients/ts/src/generated` — must be clean except the `session_api` `plugins` field (which is expected + committed). Confirm the web `src/generated/plugins/` package exists.
- [ ] **Step 6:** Commit: `git commit -am "plugins: fluorite plugins.fl + CreateSessionRequest.plugins + regen"`.

### Task 4: Capability token (`server/src/plugins/token.rs`)

**Files:**
- Create: `server/src/plugins/token.rs`
- Modify: `server/src/lib.rs` (add `pub mod plugins;`), `server/src/plugins/mod.rs`
- Test: inline `#[cfg(test)]` in `token.rs`

**Interfaces:**
- Produces: `PluginToken` with `fn sign(secret: &[u8], session_id: &str, hashes: &[String], ttl_secs: u64) -> String` and `fn verify(secret: &[u8], token: &str, hash: &str) -> Result<(), String>` (verifies signature, exp, and `hash ∈ claims.hashes`).

- [ ] **Step 1:** Write failing test in `token.rs`:
```rust
#[test]
fn sign_then_verify_allows_listed_hash_and_rejects_others() {
    let secret = b"test-secret";
    let t = sign(secret, "sess-1", &["aaa".into(), "bbb".into()], 60);
    assert!(verify(secret, &t, "aaa").is_ok());
    assert!(verify(secret, &t, "zzz").is_err());          // not listed
    assert!(verify(b"other", &t, "aaa").is_err());        // bad signature
}
```
- [ ] **Step 2:** `cargo test -p horsie-server token::` — expect FAIL (functions absent).
- [ ] **Step 3:** Implement with `jsonwebtoken` (HS256). Claims: `#[derive(Serialize,Deserialize)] struct Claims { sub: String, hashes: Vec<String>, exp: usize }`. `sign` uses `encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(secret))`; `verify` uses `decode::<Claims>` with `Validation::new(HS256)` (exp checked automatically), then `claims.hashes.iter().any(|h| h==hash)`. Compute `exp` from `SystemTime::now()+ttl` (seconds since epoch).
- [ ] **Step 4:** `cargo test -p horsie-server token::` — expect PASS.
- [ ] **Step 5:** Commit: `git commit -am "plugins: HS256 capability token"`.

### Task 5: Artifact store (`server/src/plugins/artifact.rs`)

**Files:**
- Create: `server/src/plugins/artifact.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Produces: `ArtifactStore { dir: PathBuf }` with `fn write(&self, hash: &str, bytes: &[u8]) -> io::Result<()>` (atomic: temp+rename), `fn path(&self, hash: &str) -> PathBuf`, `fn exists(&self, hash) -> bool`, `fn gc(&self, keep: &HashSet<String>) -> io::Result<()>` (delete `<hash>.zip` not in `keep`).

- [ ] **Step 1:** Failing test: write two hashes, `gc` keeping one deletes the other; `path`/`exists` correct.
- [ ] **Step 2:** `cargo test -p horsie-server artifact::` — FAIL.
- [ ] **Step 3:** Implement (create dir on `new`; `write` to `<hash>.zip.tmp` then rename; `gc` scans dir for `*.zip`, deletes those whose stem ∉ keep).
- [ ] **Step 4:** `cargo test -p horsie-server artifact::` — PASS.
- [ ] **Step 5:** Commit.

### Task 6: PluginStore (`server/src/plugins/store.rs`)

**Files:**
- Create: `server/src/plugins/store.rs`
- Test: inline `#[cfg(test)]` opening a temp SQLite pool (mirror config/store.rs test setup: `SqlitePool::connect("sqlite::memory:")` then `sqlx::migrate!().run(&pool)`).

**Interfaces:**
- Produces: `PluginStore { pool: SqlitePool }` with:
  - `async fn list(&self) -> sqlx::Result<Vec<PluginRow>>`
  - `async fn get(&self, name: &str) -> sqlx::Result<Option<PluginRow>>`
  - `async fn upsert(&self, row: &PluginRow) -> sqlx::Result<()>` (INSERT … ON CONFLICT(name) DO UPDATE)
  - `async fn set_default(&self, name: &str, enabled: bool) -> sqlx::Result<()>`
  - `async fn delete(&self, name: &str) -> sqlx::Result<()>`
  - `async fn referenced_hashes(&self) -> sqlx::Result<HashSet<String>>` (for GC)
- `PluginRow` mirrors the table columns (bool as i64 in SQL, exposed as bool).

- [ ] **Step 1:** Failing test `upsert_get_list_delete_roundtrip`: open in-memory pool + migrate; upsert a row; `get`/`list` return it; `set_default` flips the flag; `delete` removes it; `referenced_hashes` reflects rows.
- [ ] **Step 2:** `cargo test -p horsie-server plugins::store` — FAIL.
- [ ] **Step 3:** Implement with `sqlx::query`/`query_as` (runtime queries; no `DATABASE_URL` needed — matches the config store convention). Map `INTEGER` bool columns via `i64` ↔ `bool`.
- [ ] **Step 4:** `cargo test -p horsie-server plugins::store` — PASS.
- [ ] **Step 5:** Commit.

### Task 7: Ingest (`server/src/plugins/ingest.rs`)

**Files:**
- Create: `server/src/plugins/ingest.rs`
- Test: inline `#[cfg(test)]` using a `file://` git fixture built in a tempdir (`git init`, add a plugin tree, commit).

**Interfaces:**
- Produces: `async fn ingest_git(url: &str, git_ref: Option<&str>) -> Result<Ingested, String>` where `Ingested { name, version, description, skill_count, has_hooks, zip_bytes, hash }`.
- Helpers (pure, testable): `fn inspect_plugin_dir(root: &Path) -> PluginInfo { name, version, description, skill_count, has_hooks }` (read `.claude-plugin/plugin.json` if present; resolve skills location from manifest `skills` string/array else `skills/`; glob `<loc>/*/SKILL.md`; `has_hooks` = a `SessionStart` entry exists in `hooks/hooks.json` or manifest `hooks`); `fn zip_dir(root: &Path) -> Result<Vec<u8>,String>` (deterministic: sorted entries, fixed mtime) ; `fn sha256_hex(bytes: &[u8]) -> String`.

- [ ] **Step 1:** Failing test `inspect_reads_manifest_and_counts_skills`: build a temp dir with `.claude-plugin/plugin.json` (`{"name":"demo","version":"1.0.0","description":"d"}`) + `skills/a/SKILL.md` + `skills/b/SKILL.md` + `hooks/hooks.json` with a SessionStart entry; assert `inspect_plugin_dir` → name=demo, skill_count=2, has_hooks=true. Second test `zip_is_deterministic`: `zip_dir` of the same tree twice → identical bytes → identical `sha256_hex`. Third test `ingest_git_clones_fixture`: `git init` a bare-ish fixture with the tree, `ingest_git("file://…", None)` → `Ingested` with skill_count=2 and a non-empty hash.
- [ ] **Step 2:** `cargo test -p horsie-server plugins::ingest` — FAIL.
- [ ] **Step 3:** Implement. Clone: `std::process::Command::new("git").args(["clone","--depth","1"]).arg(url)` (+ `--branch <ref>` when set) into a `tempfile::tempdir()`; on non-zero exit return the stderr as `Err`. Locate root = repo root (v1: expect `.claude-plugin/plugin.json` or a `skills/` dir at root; else `Err("not a plugin: no skills found")`). `inspect_plugin_dir`. `zip_dir` using the `zip` crate (`ZipWriter`, walk sorted, `FileOptions` with a fixed `last_modified_time`). Resolve version: manifest `version` else the cloned commit sha (`git rev-parse HEAD`). Reject `skill_count == 0`.
- [ ] **Step 4:** `cargo test -p horsie-server plugins::ingest` — PASS.
- [ ] **Step 5:** Commit.

### Task 8: PluginService + PluginProvisioner (`server/src/plugins/service.rs`, `mod.rs`)

**Files:**
- Create: `server/src/plugins/service.rs`
- Modify: `server/src/plugins/mod.rs` (declare submodules; define `PluginArtifactRef`, `PluginProvisioner`)
- Test: inline `#[cfg(test)]` (temp pool + temp artifact dir; install from a `file://` fixture; resolve; mint+verify).

**Interfaces:**
- Produces:
  - `pub struct PluginArtifactRef { pub name: String, pub hash: String, pub url: String }`
  - `#[async_trait] pub trait PluginProvisioner: Send + Sync { async fn resolve(&self, names: &[String], base_url: &str) -> Result<Vec<PluginArtifactRef>, String>; fn mint_token(&self, session_id: &str, hashes: &[String]) -> String; async fn default_names(&self) -> Vec<String>; }`
  - `pub struct PluginService { store: PluginStore, artifacts: ArtifactStore, token_secret: Vec<u8> }` with:
    - `async fn list(&self) -> Result<Vec<PluginView>, String>`
    - `async fn install(&self, input: PluginInstallInput) -> Result<PluginView, String>`
    - `async fn update(&self, name: &str) -> Result<PluginView, String>`
    - `async fn set_default(&self, name: &str, enabled: bool) -> Result<PluginView, String>`
    - `async fn remove(&self, name: &str) -> Result<(), String>`
    - `fn artifact_path(&self, hash: &str) -> PathBuf` and `fn verify_token(&self, token: &str, hash: &str) -> Result<(), String>` (for the artifact route)
  - `impl PluginProvisioner for PluginService`.
- Consumes: `PluginStore`, `ArtifactStore`, `token` (Task 4-7), `horsie_models::plugins::*`.

- [ ] **Step 1:** Failing test `install_then_resolve_and_token`: build service (temp pool+migrate, temp artifact dir, secret); `install` from a `file://` fixture → `PluginView` (skill_count 2); artifact file exists; `resolve(&["demo"], "http://h:1")` → one ref with url `http://h:1/api/plugins/artifacts/<hash>.zip`; `verify_token(mint_token("s",&[hash]), &hash)` Ok, wrong hash Err.
- [ ] **Step 2:** `cargo test -p horsie-server plugins::service` — FAIL.
- [ ] **Step 3:** Implement. `install`: `ingest_git` → `artifacts.write(hash,bytes)` → `store.upsert(row{created_at=updated_at=now})` → GC via `artifacts.gc(store.referenced_hashes())` → build `PluginView`. `update`: read row for source, re-`ingest_git`, same write+upsert(updated_at=now)+GC. `remove`: `store.delete` + GC. `resolve`: for each name `store.get` → ref `{name, hash, url: format!("{base_url}/api/plugins/artifacts/{hash}.zip")}` (Err if a name is unknown). `mint_token`: `token::sign(&self.token_secret, session_id, hashes, 3600)`. `default_names`: rows where `enabled_default`. `now` as RFC3339 string (chrono is already available via workspace? if not, use `time`/format an ISO string from `SystemTime`; keep as TEXT).
- [ ] **Step 4:** `cargo test -p horsie-server plugins::service` — PASS.
- [ ] **Step 5:** Commit.

---

## Phase 3 — HTTP surface + AppState wiring

### Task 9: HTTP routes (`server/src/http/plugins.rs`)

**Files:**
- Create: `server/src/http/plugins.rs`
- Modify: `server/src/http/mod.rs` (`AppState.plugins: Arc<crate::plugins::PluginService>`; register routes)
- Modify: `cli/src/serve.rs` (construct `PluginService`, set `AppState.plugins`)
- Test: extend the `http/mod.rs` inline tests or `tests/` with a route test using a temp service.

**Interfaces:**
- Consumes: `AppState.plugins`, `PluginService`, `horsie_models::plugins::*`, `Api` error type.
- Produces routes: `GET /api/plugins`, `POST /api/plugins`, `POST /api/plugins/:name/update`, `PUT /api/plugins/:name`, `DELETE /api/plugins/:name`, `GET /api/plugins/artifacts/:file` (`:file` = `<hash>.zip`).

- [ ] **Step 1:** Implement handlers (mirror `http/config.rs` + `http/github.rs` style). List/install/update/set_default/remove call `state.plugins.*`, mapping `Err(String)` → `Api::unprocessable`. Artifact handler: parse `hash` from `:file` (strip `.zip`), read `Authorization: Bearer` header, `state.plugins.verify_token(token, hash)` → on Ok stream `tokio::fs::File` via `axum::body::Body::from_stream`/`tokio_util::io::ReaderStream` with `Content-Type: application/zip`; token invalid → `Api` 403; missing file → 404.
- [ ] **Step 2:** Register in `http/mod.rs` router; add `plugins` to `AppState` and to every `AppState { … }` construction (serve.rs + test harnesses — grep for `AppState {`).
- [ ] **Step 3:** In `cli/src/serve.rs`, build the service: artifact dir = `<data_dir>/plugins`; token secret = config `server.artifact_token_secret` (env `HORSIE_ARTIFACT_SECRET`) else a random 32-byte generated at startup (log a warning that URLs won't survive restart mid-provision); `PluginService::new(PluginStore::new(opened.pool.clone()), ArtifactStore::new(artifact_dir), secret)`. Wrap in `Arc`, put in `AppState.plugins` and (Task 11) `ServerDeps.plugins`.
- [ ] **Step 4:** Test `plugins_crud_over_http`: start the test server (extend `session_server_e2e` harness or an http test) with a `PluginService` over a temp pool+artifact dir; `POST /api/plugins {source_url: file://fixture}` → 201 + view; `GET` lists it; `GET /api/plugins/artifacts/<hash>.zip` with a minted bearer streams bytes, without/blank token → 403; `DELETE` → 204.
- [ ] **Step 5:** `cargo test -p horsie-server` (+ the workspace e2e crate) — PASS. Commit.

---

## Phase 4 — Session wiring + vendor env

### Task 10: `SessionSpec.plugins` + `create_session` mapping

**Files:**
- Modify: `server/src/sessions/spec.rs` (`SessionSpec.plugins: Vec<String>`)
- Modify: `server/src/http/handlers.rs` (`create_session`)

**Interfaces:**
- Consumes: `CreateSessionRequest.plugins` (Task 3). Produces: `SessionSpec.plugins`.

- [ ] **Step 1:** Add `pub plugins: Vec<String>,` to `SessionSpec` (after `hook_path`). Fix every `SessionSpec { … }` literal (grep) — tests included — to set `plugins: vec![]`.
- [ ] **Step 2:** In `create_session`, after building `spec`, set `plugins: req.plugins.unwrap_or_default()`. (If empty, `ensure_runtime` falls back to `default_names()` — see Task 11.) Also: if `!plugins.is_empty()`, force agent opt-in: set `spec.agent.use_plugins = Some(true)` when `req.plugins` non-empty (so selecting bundles surfaces them without a separate toggle).
- [ ] **Step 3:** `cargo build -p horsie-server` — expect success (all `SessionSpec` literals updated).
- [ ] **Step 4:** Commit: `git commit -am "plugins: thread selected bundle names into SessionSpec"`.

### Task 11: Vendor trait methods + `ensure_runtime` plugin env

**Files:**
- Modify: `server/src/vendor/mod.rs` (trait default methods), `server/src/vendor/local.rs`, `server/src/vendor/velos.rs`
- Modify: `server/src/sessions/spec.rs` (`ServerDeps.plugins: Option<Arc<dyn PluginProvisioner>>`)
- Modify: `server/src/sessions/session_actor.rs` (`ensure_runtime`)
- Modify: `models/fluorite/settings.fl` (VelosView/VelosInput `public_http_base`/`http_port`); `server/src/config/store.rs` (build velos with the new fields), `cli/src/serve.rs` (`ServerDeps.plugins`, local vendor base)

**Interfaces:**
- Adds to `RuntimeVendor`: `fn artifact_base_url(&self) -> Option<String> { None }`, `fn plugins_dir_for(&self, runtime_id: &str) -> Option<String> { None }`, `fn plugins_cache_dir(&self) -> Option<String> { None }`.
- Consumes: `PluginProvisioner` (Task 8), `ENV_*` consts (Task 1).

- [ ] **Step 1:** Add the three default-`None` trait methods to `RuntimeVendor`.
- [ ] **Step 2:** `LocalProcessVendor`: add field `public_http_base: Option<String>` (set in `new`/serve.rs). Override `artifact_base_url` → clone of it; `plugins_dir_for(id)` → `Some(<workspace_root>/<id>/.plugins)` string; `plugins_cache_dir` → `Some(<workspace_root>/.plugins-cache)`.
- [ ] **Step 3:** `VelosVendor`: add fields `public_http_base: Option<String>` + a container plugins path constant `"/horsie/plugins"`. `VelosVendorSettings` gains `public_http_base: Option<String>`. Override `artifact_base_url` → the base; `plugins_dir_for(_)` → `Some("/horsie/plugins".into())`; `plugins_cache_dir` → `None`. In `settings.fl` add `public_http_base: Option<String>` and `http_port: Option<u32>` to `VelosView`+`VelosInput`; in `config/store.rs` `build_velos_vendor`, compose `public_http_base` = explicit field else `format!("http://{advertise_host}:{http_port}")` when `http_port` set. Regen TS (settings.fl → clients/ts + clients/web).
- [ ] **Step 4:** `ServerDeps` gains `pub plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>`. Set in serve.rs (`Some(plugin_service.clone())`) and in every test `ServerDeps { … }` (`plugins: None`).
- [ ] **Step 5:** In `ensure_runtime`, after the GitHub-token block and before `vendor.create/attach`, add (mirrors the github block):
```rust
if let (Some(prov), Some(base)) = (&self.deps.plugins, vendor.artifact_base_url()) {
    let mut names = self.spec.plugins.clone();
    if names.is_empty() { names = prov.default_names().await; }
    if !names.is_empty() {
        let refs = prov.resolve(&names, &base).await?;
        let hashes: Vec<String> = refs.iter().map(|r| r.hash.clone()).collect();
        let token = prov.mint_token(&id, &hashes);
        let manifest = serde_json::to_string(&refs).map_err(|e| e.to_string())?;
        rt_spec.env.push(EnvVar { name: ENV_PLUGIN_MANIFEST.into(), value: manifest });
        rt_spec.env.push(EnvVar { name: ENV_PLUGINS_TOKEN.into(), value: token });
        if let Some(dir) = vendor.plugins_dir_for(&id) {
            rt_spec.env.push(EnvVar { name: ENV_PLUGINS_DIR.into(), value: dir });
        }
        if let Some(cache) = vendor.plugins_cache_dir() {
            rt_spec.env.push(EnvVar { name: ENV_PLUGINS_CACHE.into(), value: cache });
        }
    }
}
```
(`id` is the `runtime_id` string already computed in `ensure_runtime`; note `vendor` is `Arc<dyn RuntimeVendor>` from `self.vendor()?`. `PluginArtifactRef` must derive `Serialize` for the manifest JSON — add it in Task 8.) Requires `use horsie_models::{ENV_PLUGIN_MANIFEST, ENV_PLUGINS_TOKEN, ENV_PLUGINS_DIR, ENV_PLUGINS_CACHE, executor::EnvVar}`.
- [ ] **Step 6:** `cargo build -p horsie-server` + fix all `ServerDeps`/`AppState`/`SessionSpec` literals. `cargo test -p horsie-server` — PASS.
- [ ] **Step 7:** Commit: `git commit -am "plugins: vendor artifact base/dir + ensure_runtime manifest env"`.

---

## Phase 5 — Runtime fetch

### Task 12: `runtime/src/plugins_fetch.rs`

**Files:**
- Create: `runtime/src/plugins_fetch.rs`
- Modify: `runtime/src/main.rs`
- Test: inline `#[cfg(test)]` against a stub HTTP server (`tokio` + a tiny axum/`tiny_http`? — prefer a `tokio::net::TcpListener` hand-rolled 200 with the zip bytes, or reuse `reqwest` against a spawned axum route in a test util). Simplest: unit-test `unpack_zip` + `verify` pure fns; integration-test the fetch against a spawned axum server in `runtime`'s dev-deps if axum is available; otherwise test fetch via a `file://`-style path is not possible with reqwest → use a spawned `tokio` HTTP stub.

**Interfaces:**
- Produces: `pub async fn provision_plugins() -> Option<PathBuf>` — reads `ENV_PLUGIN_MANIFEST`/`ENV_PLUGINS_TOKEN`/`ENV_PLUGINS_DIR`/`ENV_PLUGINS_CACHE`; returns `Some(plugins_dir)` if it materialized ≥1 bundle, else `None`. Non-fatal: logs+skips a bundle on any error.
- Pure helpers: `fn sha256_hex(bytes:&[u8])->String`, `fn unpack_zip(bytes:&[u8], into:&Path)->Result<(),String>`.

- [ ] **Step 1:** Failing test `unpack_zip_writes_tree`: given zip bytes (built with the `zip` crate in-test) unpack into a tempdir and assert files exist; `sha256_hex` matches a known vector.
- [ ] **Step 2:** `cargo test -p horsie-runtime plugins_fetch` — FAIL.
- [ ] **Step 3:** Implement `provision_plugins`: parse manifest JSON (`Vec<{name,hash,url}>`); dir = `ENV_PLUGINS_DIR` (create it); for each entry: if cache set and `<cache>/<hash>/` exists, copy/symlink into `<dir>/<name>`; else `reqwest::Client::get(url).bearer_auth(token).send()` → bytes → verify sha256 == hash (skip+warn on mismatch) → `unpack_zip` into `<cache>/<hash>` (if cache) then link, else directly into `<dir>/<name>`. Return `Some(dir)` if any bundle landed.
- [ ] **Step 4:** In `main.rs`, during startup provisioning (before announcing `RuntimeReady`, alongside the existing provision-steps handling): `if let Some(dir) = plugins_fetch::provision_plugins().await { /* set the runtime's plugins_dir to dir */ }`. The runtime uses this dir as `plugins_dir` for `ScanWorkspace(include_shared)` — i.e. it overrides/sets the `--plugins-dir` value the scan uses. (Trace where `--plugins-dir` is stored after arg parse in `main.rs` and assign `dir` there.)
- [ ] **Step 5:** `cargo test -p horsie-runtime` — PASS. `cargo build -p horsie-runtime` — success.
- [ ] **Step 6:** Commit: `git commit -am "runtime: fetch + unpack selected plugin bundles before ready"`.

---

## Phase 6 — Web UI

### Task 13: `usePlugins` hook + `api.plugins`

**Files:**
- Modify: `clients/web/src/api/client.ts` (add `plugins` namespace)
- Create: `clients/web/src/hooks/usePlugins.ts`

**Interfaces:**
- Produces: `api.plugins.{list, install, update, setDefault, remove}`; hooks `usePlugins()`, `useInstallPlugin()`, `useUpdatePlugin()`, `useSetPluginDefault()`, `useRemovePlugin()`.

- [ ] **Step 1:** Add to `api` in `client.ts`:
```ts
plugins: {
  list: (): Promise<PluginView[]> => request("/plugins"),
  install: (body: PluginInstallInput): Promise<PluginView> => request("/plugins", { method: "POST", body: JSON.stringify(body) }),
  update: (name: string): Promise<PluginView> => request(`/plugins/${encodeURIComponent(name)}/update`, { method: "POST" }),
  setDefault: (name: string, body: PluginDefaultInput): Promise<PluginView> => request(`/plugins/${encodeURIComponent(name)}`, { method: "PUT", body: JSON.stringify(body) }),
  remove: (name: string): Promise<void> => request(`/plugins/${encodeURIComponent(name)}`, { method: "DELETE" }),
},
```
- [ ] **Step 2:** Write `usePlugins.ts` mirroring `useSettings.ts` (React Query): `pluginsKey = ["plugins"]`; `usePlugins()` = `useQuery(list)`; mutations invalidate `pluginsKey` on success.
- [ ] **Step 3:** `cd clients/web && bun run build` — expect success (type-checks). Commit.

### Task 14: SkillsPage + route + nav

**Files:**
- Create: `clients/web/src/pages/SkillsPage.tsx`
- Modify: `clients/web/src/App.tsx` (route `path="skills"`), `clients/web/src/components/Sidebar.tsx` (NavLink to `/skills`, lucide `Boxes`/`Puzzle` icon)

- [ ] **Step 1:** Build `SkillsPage` (mirror `SettingsPage` shell): header; an **Install** card (git URL + optional ref inputs + Install button calling `useInstallPlugin`); a list of `PluginView` rows (name, version, description, `skill_count` skills, a "hooks" badge when `has_hooks`, an `enabled_default` toggle calling `useSetPluginDefault`, Update + Delete buttons). Loading/error states like SettingsPage. Install errors surface `ApiRequestError.message`.
- [ ] **Step 2:** Add `<Route path="skills" element={<SkillsPage />} />` in `App.tsx`; add a `<NavLink to="/skills">` in the Sidebar footer next to Settings.
- [ ] **Step 3:** `cd clients/web && bun run build` — success. Commit.

### Task 15: NewSessionModal bundle multi-select

**Files:**
- Modify: `clients/web/src/components/NewSessionModal.tsx`

- [ ] **Step 1:** Add `const { data: plugins } = usePlugins();` and `const [selected, setSelected] = useState<Set<string>>(new Set());`. On open (in the existing reseed `useEffect`), initialize `selected` from `plugins?.filter(p=>p.enabledDefault).map(p=>p.name)`. Render (in Advanced, or as a top-level "Skills" field when `plugins?.length`): a checkbox list (mirror the GitHub repo picker markup) toggling names in `selected`.
- [ ] **Step 2:** In `submit`, set `plugins: selected.size ? [...selected] : undefined` on the `CreateSessionRequest` body. (Selecting bundles implies opt-in; the server forces `use_plugins` when `plugins` non-empty per Task 10.)
- [ ] **Step 3:** `cd clients/web && bun run build` — success. Commit.

---

## Phase 7 — Deploy + full gate + PR

### Task 16: Deploy wiring

**Files:**
- Modify: `docker/server.Dockerfile` (install `git` in the runtime image layer), `october/ops/horsie/docker-compose.yml` + `RUNBOOK.md` (config: `server.public_http_base`, `HORSIE_ARTIFACT_SECRET`; velos `public_http_base`/`http_port`; a "Installing a skill bundle" + "select at session create" note).

- [ ] **Step 1:** Add `git` to the server Dockerfile's runtime stage (`apt-get install -y --no-install-recommends git` or the distro equivalent used there). Verify the base image + pattern by reading the current Dockerfile.
- [ ] **Step 2:** Document the new config in RUNBOOK + set defaults in compose (public base = `http://192.168.68.60:3789`; velos `http_port: 3789`). `october/ops` is not a git repo — edit in place (no commit there).
- [ ] **Step 3:** Commit the Dockerfile change in the horsie worktree.

### Task 17: Full gate + PR

- [ ] **Step 1:** Run the full gate (Global Constraints). Fix until green: `cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features`; `cargo deny check`; `cd clients/ts && bun run generate-types && cd - && git diff --exit-code clients/ts/src/generated`; `cd clients/web && bun run generate-types && bun run build && git diff --exit-code src/generated`.
- [ ] **Step 2:** Push `skills-plugins`; open PR against `main` (title e.g. "Skill bundles: DB-managed, per-session, cross-vendor plugin provisioning"; body summarizing the design + linking the spec; no AI attribution). Confirm CI green.

---

## Self-review notes

- **Spec coverage:** install/update/delete/list (Tasks 7-9), per-session select (Tasks 3,10,15), cross-vendor fetch (Tasks 11-12), trust/hooks (inherited — runtime machinery unchanged), latest-at-start (Task 11 resolves live), HMAC token (Task 4), artifact serving/CDN-ready path (Task 9), web UI (Tasks 13-15), deploy/git (Task 16). Covered.
- **Descope vs spec:** the spec's "factor plugin-inspection out of runtime/src/plugins.rs into a shared helper" is replaced by a minimal server-side `inspect_plugin_dir` (Task 7) to avoid a cross-crate refactor; unifying is a noted follow-up.
- **Reconciliation:** existing `agent.usePlugins` toggle stays; selecting bundles forces `use_plugins=true` server-side (Task 10) so selection alone surfaces them.
- **Known integration risks to watch during execution:** (1) every `AppState`/`ServerDeps`/`SessionSpec` struct-literal (incl. test harnesses) must gain the new fields — grep and fix or the workspace won't compile; (2) `PluginArtifactRef` needs `Serialize` for the manifest; (3) runtime `main.rs` — confirm exactly where the parsed `--plugins-dir` is held so the fetched dir can override it; (4) ts-drift: regen `clients/ts` after the `session_api.fl` + `settings.fl` edits.
