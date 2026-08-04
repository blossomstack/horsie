# Environments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an experimental first-class "environment" concept (named runtime vendor + repos + env vars + provision steps) with CRUD API, DB persistence, and web UI — no wiring into sessions/agents/routines.

**Architecture:** Mirrors the agents stack exactly: fluorite wire schema → sqlite/postgres migration → `server/src/environments/{store,service}` → `server/src/http/environments.rs` → web API client/hooks → list + edit pages + sidebar link. Storage types are hand-written twins of the wire types (protocol types are not storage types); list-typed columns are JSON text.

**Tech Stack:** Rust (axum, sqlx `Any`), fluorite codegen, React + TypeScript (react-query, react-router, vitest).

**Spec:** `docs/superpowers/specs/2026-08-04-environments-design.md`

## Global Constraints

- Work in the worktree branch `feat/environments` (already created).
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`; test modules opt out with `#[cfg(test)] #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`.
- `vendor` is required and must not be `"local"` (case-sensitive); validated in the service layer.
- Env vars are plain text, non-sensitive; no redaction.
- No session/agent/routine may reference an environment in this change.
- Server errors use the `Api` envelope: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
- Test commands: `cargo test --workspace` (single-crate `-p` tests fail on feature gating — see CLAUDE.md memory; for iteration use `cargo test --workspace <test-name>` filters or accept the longer run). Format with stable toolchain only.

---

### Task 1: Fluorite schema `environments.fl` + generated types (Rust & TS)

**Files:**
- Create: `models/fluorite/environments.fl`
- Modify: `models/src/lib.rs` (add `pub mod environments` after the `agents` module block)
- Modify: `clients/web/package.json` (`generate-types` script: add `environments.fl` and `executor.fl` to the `-i` list)
- Modify: `clients/web/src/api/types.ts` (export the two new generated modules)

**Interfaces:**
- Produces: `horsie_models::environments::{EnvironmentView, EnvironmentInput}` (Rust); `EnvironmentView`, `EnvironmentInput`, `EnvVar`, `ProvisionStep`, `StepParam` (TS, via `api/types.ts`). Wire JSON is camelCase (`gitRef`, `envVars`). Fields (exact):
  - `EnvironmentView { name: string, description: string, vendor: string, repos: RepoConfig[], envVars: EnvVar[], provision: ProvisionStep[], createdAt: string, updatedAt: string }`
  - `EnvironmentInput { name: string, description?: string, vendor: string, repos?: RepoConfig[], envVars?: EnvVar[], provision?: ProvisionStep[] }`

- [ ] **Step 1: Write the schema**

Create `models/fluorite/environments.fl`:

```florite
/// Named environments (experimental): a reusable runtime + repos bundle. An
/// environment names its runtime vendor — the opposite pole from agent
/// presets, which deliberately name no vendor. Nothing consumes an
/// environment yet: sessions, presets, and routines do not reference one.
package environments;

use session_api.RepoConfig;
use executor.EnvVar;
use executor.ProvisionStep;

/// An environment as shown to clients.
struct EnvironmentView {
    /// Slug; the id of record, used in API paths.
    name: String,
    description: String,
    /// Runtime vendor name. Required, and never "local": environments only
    /// target vendor-managed, provisionable runtimes.
    vendor: String,
    /// Repositories cloned into the runtime workspace at provision time.
    repos: Vec<RepoConfig>,
    /// Plain-text, non-sensitive env vars for the runtime. Secrets are a
    /// future, separate concept.
    env_vars: Vec<EnvVar>,
    /// Setup steps the runtime executes before its message loop. Inert today:
    /// nothing provisions from an environment yet.
    provision: Vec<ProvisionStep>,
    /// Unix epoch seconds.
    created_at: String,
    updated_at: String,
}

/// Create or fully replace an environment. Omitted list fields default to
/// empty; `description` defaults to "".
struct EnvironmentInput {
    name: String,
    description: Option<String>,
    vendor: String,
    repos: Option<Vec<RepoConfig>>,
    env_vars: Option<Vec<EnvVar>>,
    provision: Option<Vec<ProvisionStep>>,
}
```

- [ ] **Step 2: Register the Rust module**

In `models/src/lib.rs`, add after the `pub mod agents { ... }` block (keeping the file's existing ordering):

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod environments {
    include!(concat!(env!("OUT_DIR"), "/environments/mod.rs"));
}
```

- [ ] **Step 3: Verify Rust codegen compiles**

Run: `cargo build -p horsie-models`
Expected: builds; `horsie_models::environments::EnvironmentView` exists.

- [ ] **Step 4: Generate the TS types**

In `clients/web/package.json`, add `../../models/fluorite/executor.fl` and `../../models/fluorite/environments.fl` to the `generate-types` script's `-i` list (order: keep alphabetical-ish, matching the existing list). Then:

Run: `cd clients/web && bun run generate-types` (or `npm run generate-types`)
Expected: `src/generated/environments/` and `src/generated/executor/` appear.

If the generated `environments` module fails to resolve its `executor` imports without `executor.fl` in the inputs, that is why executor is included — keep both.

- [ ] **Step 5: Export from the types surface**

In `clients/web/src/api/types.ts`, add (keeping the existing ordering, after `capabilities` / before `github`):

```ts
export * from "../generated/environments";
export * from "../generated/executor";
```

- [ ] **Step 6: Verify TS compiles**

Run: `cd clients/web && bun run typecheck`
Expected: no errors. Watch for name collisions between the newly exported `executor` types (`EnvVar`, `ProvisionStep`, `StepParam`, `WorkspaceConfig`, `RuntimeInfo`, `RuntimeState`) and existing exports — there should be none; if there is one, do not blanket-export `executor`; export only what's needed with explicit named exports.

- [ ] **Step 7: Commit**

```bash
git add models/fluorite/environments.fl models/src/lib.rs clients/web/package.json clients/web/src/api/types.ts clients/web/src/generated
git commit -m "feat(models): environments wire schema"
```

---

### Task 2: Migration `0018_environments.sql` (sqlite + postgres)

**Files:**
- Create: `server/migrations/sqlite/0018_environments.sql`
- Create: `server/migrations/postgres/0018_environments.sql`

**Interfaces:**
- Produces: table `environments(name TEXT PK, description TEXT, vendor TEXT, repos TEXT JSON, env_vars TEXT JSON, provision TEXT JSON, created_at TEXT, updated_at TEXT)`.

- [ ] **Step 1: Write both migration files**

`server/migrations/sqlite/0018_environments.sql`:

```sql
-- Named environments (experimental): a reusable runtime + repos bundle. Nothing
-- references one yet — this is the first step of the environments exploration.
-- List-typed columns are JSON arrays; `repos` elements are
-- {"url", "git_ref"?, "dir"?}, `env_vars` are {"name", "value"}, `provision`
-- elements are {"name", "uses", "with": [{"key", "value"}]}.

CREATE TABLE environments (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    vendor      TEXT NOT NULL,
    repos       TEXT NOT NULL DEFAULT '[]',
    env_vars    TEXT NOT NULL DEFAULT '[]',
    provision   TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);
```

`server/migrations/postgres/0018_environments.sql`: identical body, prefixed with the mirror header line used by the other postgres files:

```sql
-- PostgreSQL mirror of migrations/sqlite/0018_environments.sql.
--
-- Named environments (experimental): a reusable runtime + repos bundle. Nothing
-- references one yet — this is the first step of the environments exploration.
-- List-typed columns are JSON arrays; `repos` elements are
-- {"url", "git_ref"?, "dir"?}, `env_vars` are {"name", "value"}, `provision`
-- elements are {"name", "uses", "with": [{"key", "value"}]}.

CREATE TABLE environments (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    vendor      TEXT NOT NULL,
    repos       TEXT NOT NULL DEFAULT '[]',
    env_vars    TEXT NOT NULL DEFAULT '[]',
    provision   TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);
```

- [ ] **Step 2: Run the migration tests**

Run: `cargo test --workspace migrations`
Expected: PASS — `server/src/db/mod.rs` has `migrations_are_in_parity` and duplicate-version tests that now cover 0018, and `db::testing::db()` (used across the suite) applies it.

- [ ] **Step 3: Commit**

```bash
git add server/migrations
git commit -m "feat(server): environments table migration"
```

---

### Task 3: `EnvironmentStore` — storage types + SQL

**Files:**
- Create: `server/src/environments/mod.rs`
- Create: `server/src/environments/store.rs`
- Modify: `server/src/lib.rs` (add `pub mod environments;` after `pub mod db;`)

**Interfaces:**
- Consumes: `crate::db::Db` (`.q()`, `.pool()`), `crate::db::testing::db()` in tests.
- Produces (used by Task 4):
  - `EnvironmentRepo { url: String, git_ref: Option<String>, dir: Option<String> }` — serde snake_case, `skip_serializing_if = "Option::is_none"` on the options.
  - `EnvironmentEnvVar { name: String, value: String }`
  - `EnvironmentStepParam { key: String, value: String }`
  - `EnvironmentProvisionStep { name: String, uses: String, with: Vec<EnvironmentStepParam> }` (`#[serde(default)]` on `with`)
  - `EnvironmentRow { name, description, vendor, repos: Vec<EnvironmentRepo>, env_vars: Vec<EnvironmentEnvVar>, provision: Vec<EnvironmentProvisionStep>, created_at, updated_at }`
  - `EnvironmentStore::new(db: Db)`, `list() -> Result<Vec<EnvironmentRow>, String>`, `get(name) -> Result<Option<EnvironmentRow>, String>`, `insert(&EnvironmentRow) -> Result<(), String>` (errs on duplicate), `replace(&EnvironmentRow) -> Result<bool, String>` (false on miss), `delete(name) -> Result<bool, String>`.

- [ ] **Step 1: Write the failing test module skeleton**

Create `server/src/environments/store.rs` with the storage types and a `EnvironmentStore` whose methods are `todo!()`-free but minimal — write the tests first (below), watch them fail to compile/run, then implement. The full test suite for this file (adapted from `agents/store.rs`):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> EnvironmentStore {
        EnvironmentStore::new(crate::db::testing::db().await)
    }

    fn row(name: &str) -> EnvironmentRow {
        EnvironmentRow {
            name: name.into(),
            description: "d".into(),
            vendor: "fly".into(),
            repos: vec![EnvironmentRepo {
                url: "https://github.com/o/api".into(),
                git_ref: Some("dev".into()),
                dir: None,
            }],
            env_vars: vec![EnvironmentEnvVar {
                name: "RUST_LOG".into(),
                value: "debug".into(),
            }],
            provision: vec![EnvironmentProvisionStep {
                name: "install deps".into(),
                uses: "run".into(),
                with: vec![EnvironmentStepParam {
                    key: "cmd".into(),
                    value: "make setup".into(),
                }],
            }],
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_json_columns() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got, row("a"));
        assert_eq!(got.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(got.provision[0].with[0].key, "cmd");
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_lists_round_trip() {
        let s = store().await;
        let mut r = row("a");
        r.repos = vec![];
        r.env_vars = vec![];
        r.provision = vec![];
        s.insert(&r).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert!(got.repos.is_empty() && got.env_vars.is_empty() && got.provision.is_empty());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.insert(&row("a")).await.is_err());
    }

    #[tokio::test]
    async fn replace_updates_and_reports_misses() {
        let s = store().await;
        assert!(!s.replace(&row("ghost")).await.unwrap());
        s.insert(&row("a")).await.unwrap();
        let mut r = row("a");
        r.vendor = "docker".into();
        r.updated_at = "2".into();
        assert!(s.replace(&r).await.unwrap());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.vendor, "docker");
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn a_corrupt_json_column_is_an_error_not_a_default() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        sqlx::query("UPDATE environments SET repos = 'not json' WHERE name = 'a'")
            .execute(s.db_pool_for_test())
            .await
            .unwrap();
        let err = s.get("a").await.unwrap_err();
        assert!(err.contains("repos"), "{err}");
    }
}
```

Note: `db_pool_for_test` does not exist yet — see step 3. Alternative: keep the raw `Db` clone in the test as `agents/store.rs` tests do (`store() -> (EnvironmentStore, Db)`). Follow the agents test pattern exactly: return the tuple and use `db.pool()`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace environments::store`
Expected: FAIL (module doesn't compile / methods missing).

- [ ] **Step 3: Implement the store**

`server/src/environments/store.rs` — mirror `server/src/agents/store.rs` (same imports, same `to_json`/`from_json` helpers with `"environments.{col}"` error prefix, same `row_to_*` shape):

```rust
//! Storage for environments, sharing the config store's database.
//! List-typed columns are JSON; the types below are storage twins of the wire
//! `session_api::RepoConfig`, `executor::EnvVar`, and `executor::ProvisionStep`
//! (protocol types are not storage types).

use crate::db::Db;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str =
    "name, description, vendor, repos, env_vars, provision, created_at, updated_at";

/// One repo to clone at provision time (storage twin of wire `RepoConfig`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentRepo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// One plain-text env var (storage twin of wire `executor::EnvVar`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentEnvVar {
    pub name: String,
    pub value: String,
}

/// One key/value parameter of a provision step (storage twin of `StepParam`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentStepParam {
    pub key: String,
    pub value: String,
}

/// One setup step (storage twin of wire `executor::ProvisionStep`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentProvisionStep {
    pub name: String,
    pub uses: String,
    #[serde(default)]
    pub with: Vec<EnvironmentStepParam>,
}

/// One row of the `environments` table.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentRow {
    pub name: String,
    pub description: String,
    pub vendor: String,
    pub repos: Vec<EnvironmentRepo>,
    pub env_vars: Vec<EnvironmentEnvVar>,
    pub provision: Vec<EnvironmentProvisionStep>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct EnvironmentStore {
    db: Db,
}

impl EnvironmentStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<EnvironmentRow>, String> {
        let rows = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM environments ORDER BY name")),
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_environment).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<EnvironmentRow>, String> {
        let row = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM environments WHERE name = ?")),
        )
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_environment).transpose()
    }

    /// Insert; errs when the name is taken (no upsert -- a silent overwrite
    /// would discard the existing environment).
    pub async fn insert(&self, row: &EnvironmentRow) -> Result<(), String> {
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO environments ({COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.env_vars)?)
        .bind(to_json(&row.provision)?)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create environment '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no environment has that name.
    pub async fn replace(&self, row: &EnvironmentRow) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(
            "UPDATE environments SET description = ?, vendor = ?, repos = ?, \
             env_vars = ?, provision = ?, updated_at = ? WHERE name = ?",
        ))
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.env_vars)?)
        .bind(to_json(&row.provision)?)
        .bind(&row.updated_at)
        .bind(&row.name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM environments WHERE name = ?"),
        )
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

fn from_json<T: serde::de::DeserializeOwned>(col: &str, text: String) -> Result<T, String> {
    serde_json::from_str(&text).map_err(|e| format!("environments.{col}: {e}"))
}

fn row_to_environment(row: &AnyRow) -> Result<EnvironmentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(EnvironmentRow {
        name: get("name")?,
        description: get("description")?,
        vendor: get("vendor")?,
        repos: from_json("repos", get("repos")?)?,
        env_vars: from_json("env_vars", get("env_vars")?)?,
        provision: from_json("provision", get("provision")?)?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}
```

`server/src/environments/mod.rs`:

```rust
//! Named environments (experimental): a reusable runtime + repos bundle.
//! Mirrors the `agents` module's store/service split. Row types are
//! hand-written storage types; the fluorite wire types in
//! `horsie_models::environments` are mapped at the service boundary.

mod service;
mod store;

pub use service::{EnvironmentError, EnvironmentService};
pub use store::{EnvironmentRow, EnvironmentStore};
```

(In Task 4 the `pub use store::` line grows to include the twin types if the service needs them outside the module — the service is a sibling module, so `pub(crate)` visibility via the module tree suffices; export only what `http/` and `main.rs` name.)

Add to `server/src/lib.rs` after `pub mod db;`:

```rust
pub mod environments;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace environments::store`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add server/src/environments server/src/lib.rs
git commit -m "feat(server): environments store"
```

---

### Task 4: `EnvironmentService` — validation + wire mapping

**Files:**
- Create: `server/src/environments/service.rs`
- Modify: `server/src/environments/mod.rs` (re-exports)

**Interfaces:**
- Consumes: Task 3's `EnvironmentStore` + row types; `horsie_models::environments::{EnvironmentView, EnvironmentInput}`; `horsie_models::session_api::RepoConfig`; `horsie_models::executor::{EnvVar, ProvisionStep, StepParam}`; `crate::memory::validate_slug`.
- Produces (used by Task 5):
  - `EnvironmentError::{NotFound, Conflict, Invalid, Internal}(String)` — `Display` writes the message, same shape as `AgentError`.
  - `EnvironmentService::new(store: EnvironmentStore)`; `list() -> Result<Vec<EnvironmentView>, EnvironmentError>`; `get(name) -> Result<EnvironmentView, EnvironmentError>`; `create(EnvironmentInput) -> Result<EnvironmentView, EnvironmentError>`; `replace(name, EnvironmentInput) -> Result<EnvironmentView, EnvironmentError>`; `delete(name) -> Result<(), EnvironmentError>`.
  - Validation: `validate_slug(name)`; `vendor.trim()` must be non-empty and `!= "local"`; stored vendor is the trimmed value. `replace` rejects `input.name != name` (path is the id of record).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn service() -> EnvironmentService {
        EnvironmentService::new(EnvironmentStore::new(crate::db::testing::db().await))
    }

    fn input(name: &str, vendor: &str) -> EnvironmentInput {
        EnvironmentInput {
            name: name.into(),
            description: Some("d".into()),
            vendor: vendor.into(),
            repos: None,
            env_vars: None,
            provision: None,
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_defaults_and_timestamps() {
        let s = service().await;
        let v = s.create(input("a", "fly")).await.unwrap();
        assert_eq!(v.name, "a");
        assert_eq!(v.vendor, "fly");
        assert!(v.repos.is_empty() && v.env_vars.is_empty() && v.provision.is_empty());
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn create_validates_slug_and_vendor() {
        let s = service().await;
        assert!(matches!(
            s.create(input("Not A Slug", "fly")).await.unwrap_err(),
            EnvironmentError::Invalid(_)
        ));
        for bad_vendor in ["", "   ", "local"] {
            let err = s.create(input("a", bad_vendor)).await.unwrap_err();
            assert!(
                matches!(err, EnvironmentError::Invalid(ref m) if m.contains("vendor")),
                "{bad_vendor:?}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn vendor_is_stored_trimmed() {
        let s = service().await;
        let v = s.create(input("a", "  fly  ")).await.unwrap();
        assert_eq!(v.vendor, "fly");
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let s = service().await;
        s.create(input("a", "fly")).await.unwrap();
        assert!(matches!(
            s.create(input("a", "fly")).await.unwrap_err(),
            EnvironmentError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn replace_swaps_fields_and_keeps_created_at() {
        let s = service().await;
        let v = s.create(input("a", "fly")).await.unwrap();
        let mut upd = input("a", "docker");
        upd.description = Some("new".into());
        let got = s.replace("a", upd).await.unwrap();
        assert_eq!(got.vendor, "docker");
        assert_eq!(got.description, "new");
        assert_eq!(got.created_at, v.created_at);
        // Rename via body → invalid; unknown → not found.
        assert!(matches!(
            s.replace("a", input("b", "fly")).await.unwrap_err(),
            EnvironmentError::Invalid(_)
        ));
        assert!(matches!(
            s.replace("ghost", input("ghost", "fly")).await.unwrap_err(),
            EnvironmentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn delete_and_get_report_unknown_names() {
        let s = service().await;
        assert!(matches!(s.get("ghost").await.unwrap_err(), EnvironmentError::NotFound(_)));
        assert!(matches!(s.delete("ghost").await.unwrap_err(), EnvironmentError::NotFound(_)));
        s.create(input("a", "fly")).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(matches!(s.get("a").await.unwrap_err(), EnvironmentError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_is_ordered_by_name() {
        let s = service().await;
        s.create(input("b", "fly")).await.unwrap();
        s.create(input("a", "fly")).await.unwrap();
        let names: Vec<String> = s.list().await.unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn repos_env_vars_and_provision_round_trip_through_the_mapping() {
        let s = service().await;
        let mut i = input("a", "fly");
        i.repos = Some(vec![RepoConfig {
            url: "https://github.com/o/api".into(),
            git_ref: Some("dev".into()),
            dir: None,
        }]);
        i.env_vars = Some(vec![EnvVar { name: "RUST_LOG".into(), value: "debug".into() }]);
        i.provision = Some(vec![ProvisionStep {
            name: "setup".into(),
            uses: "run".into(),
            with: vec![StepParam { key: "cmd".into(), value: "make setup".into() }],
        }]);
        let v = s.create(i).await.unwrap();
        assert_eq!(v.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(v.env_vars[0].name, "RUST_LOG");
        assert_eq!(v.provision[0].with[0].value, "make setup");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace environments::service`
Expected: FAIL (no `service` module / `EnvironmentService`).

- [ ] **Step 3: Implement the service**

`server/src/environments/service.rs` (structure mirrors `agents/service.rs`; unlike agents there is no `ConfigStore` dependency — nothing here validates against live settings):

```rust
//! Validation, timestamps, and row↔wire mapping over `EnvironmentStore`.
//! Save-time validation covers only what's stable at save: the name slug and
//! the vendor rule (required, never "local"). Whether the named vendor is
//! connected is a runtime concern — an environment can outlive the vendor it
//! names.

use crate::environments::store::{
    EnvironmentEnvVar, EnvironmentProvisionStep, EnvironmentRepo, EnvironmentRow,
    EnvironmentStepParam, EnvironmentStore,
};
use horsie_models::environments::{EnvironmentInput, EnvironmentView};
use horsie_models::executor::{EnvVar, ProvisionStep, StepParam};
use horsie_models::session_api::RepoConfig;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum EnvironmentError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for EnvironmentError {}

pub struct EnvironmentService {
    store: EnvironmentStore,
}

impl EnvironmentService {
    pub fn new(store: EnvironmentStore) -> Self {
        Self { store }
    }

    pub async fn list(&self) -> Result<Vec<EnvironmentView>, EnvironmentError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(EnvironmentError::Internal)?
            .iter()
            .map(environment_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<EnvironmentView, EnvironmentError> {
        self.store
            .get(name)
            .await
            .map_err(EnvironmentError::Internal)?
            .as_ref()
            .map(environment_view)
            .ok_or_else(|| EnvironmentError::NotFound(format!("unknown environment '{name}'")))
    }

    pub async fn create(
        &self,
        input: EnvironmentInput,
    ) -> Result<EnvironmentView, EnvironmentError> {
        let vendor = validate(&input)?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(EnvironmentError::Internal)?
            .is_some()
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment '{}' already exists",
                input.name
            )));
        }
        let now = now_secs();
        let row = row_from_input(input, vendor, now.clone(), now);
        self.store
            .insert(&row)
            .await
            .map_err(EnvironmentError::Internal)?;
        self.get(&row.name).await
    }

    /// Full replace. The path name is the id of record: a body naming a
    /// different environment is invalid rather than a rename.
    pub async fn replace(
        &self,
        name: &str,
        input: EnvironmentInput,
    ) -> Result<EnvironmentView, EnvironmentError> {
        if input.name != name {
            return Err(EnvironmentError::Invalid(
                "environment name is immutable; the path is the id of record".to_string(),
            ));
        }
        let existing = self
            .store
            .get(name)
            .await
            .map_err(EnvironmentError::Internal)?
            .ok_or_else(|| EnvironmentError::NotFound(format!("unknown environment '{name}'")))?;
        let vendor = validate(&input)?;
        let row = row_from_input(input, vendor, existing.created_at, now_secs());
        self.store
            .replace(&row)
            .await
            .map_err(EnvironmentError::Internal)?;
        self.get(name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), EnvironmentError> {
        if self
            .store
            .delete(name)
            .await
            .map_err(EnvironmentError::Internal)?
        {
            Ok(())
        } else {
            Err(EnvironmentError::NotFound(format!(
                "unknown environment '{name}'"
            )))
        }
    }
}

/// Save-time validation; returns the trimmed vendor to store.
fn validate(input: &EnvironmentInput) -> Result<String, EnvironmentError> {
    crate::memory::validate_slug(&input.name).map_err(EnvironmentError::Invalid)?;
    let vendor = input.vendor.trim();
    if vendor.is_empty() {
        return Err(EnvironmentError::Invalid(
            "vendor must not be empty: an environment names the runtime it runs on".to_string(),
        ));
    }
    if vendor == "local" {
        return Err(EnvironmentError::Invalid(
            "vendor 'local' is not supported: environments target vendor-managed runtimes"
                .to_string(),
        ));
    }
    Ok(vendor.to_string())
}

fn row_from_input(
    input: EnvironmentInput,
    vendor: String,
    created_at: String,
    updated_at: String,
) -> EnvironmentRow {
    EnvironmentRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        vendor,
        repos: input
            .repos
            .unwrap_or_default()
            .into_iter()
            .map(|r| EnvironmentRepo {
                url: r.url,
                git_ref: r.git_ref,
                dir: r.dir,
            })
            .collect(),
        env_vars: input
            .env_vars
            .unwrap_or_default()
            .into_iter()
            .map(|v| EnvironmentEnvVar {
                name: v.name,
                value: v.value,
            })
            .collect(),
        provision: input
            .provision
            .unwrap_or_default()
            .into_iter()
            .map(|p| EnvironmentProvisionStep {
                name: p.name,
                uses: p.uses,
                with: p
                    .with
                    .into_iter()
                    .map(|w| EnvironmentStepParam {
                        key: w.key,
                        value: w.value,
                    })
                    .collect(),
            })
            .collect(),
        created_at,
        updated_at,
    }
}

fn environment_view(row: &EnvironmentRow) -> EnvironmentView {
    EnvironmentView {
        name: row.name.clone(),
        description: row.description.clone(),
        vendor: row.vendor.clone(),
        repos: row
            .repos
            .iter()
            .map(|r| RepoConfig {
                url: r.url.clone(),
                git_ref: r.git_ref.clone(),
                dir: r.dir.clone(),
            })
            .collect(),
        env_vars: row
            .env_vars
            .iter()
            .map(|v| EnvVar {
                name: v.name.clone(),
                value: v.value.clone(),
            })
            .collect(),
        provision: row
            .provision
            .iter()
            .map(|p| ProvisionStep {
                name: p.name.clone(),
                uses: p.uses.clone(),
                with: p
                    .with
                    .iter()
                    .map(|w| StepParam {
                        key: w.key.clone(),
                        value: w.value.clone(),
                    })
                    .collect(),
            })
            .collect(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    }
}

fn now_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}
```

Note for the implementer: if the generated `RepoConfig`/`EnvVar`/`ProvisionStep` structs use `derive_new` constructors, field-init syntax still works since all fields are `pub` (that is how `agents/service.rs` builds `RepoConfig`). Also note the test module needs `use horsie_models::session_api::RepoConfig;` etc. in scope — they're imported at the top of the file, and tests do `use super::*;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace environments::service`
Expected: PASS (8 tests).

- [ ] **Step 5: Commit**

```bash
git add server/src/environments
git commit -m "feat(server): environments service"
```

---

### Task 5: HTTP handlers, routes, state, and server wiring

**Files:**
- Create: `server/src/http/environments.rs`
- Modify: `server/src/http/mod.rs` (module decl, `AppState` field, routes, `test_state`)
- Modify: `server/src/bin/horsie-server/main.rs` (construct service, add to `AppState`)

**Interfaces:**
- Consumes: Task 4's `EnvironmentService`/`EnvironmentError`; `super::error::Api`.
- Produces: routes `GET/POST /api/environments`, `GET/PUT/DELETE /api/environments/:name`; `AppState.environments: Arc<EnvironmentService>`.

- [ ] **Step 1: Write the handlers**

`server/src/http/environments.rs`:

```rust
//! HTTP surface for environments: CRUD for the web UI. There is no invoke or
//! run endpoint — nothing consumes an environment yet.

use super::AppState;
use super::error::Api;
use crate::environments::EnvironmentError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::environments::{EnvironmentInput, EnvironmentView};

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: EnvironmentError) -> Api {
    match e {
        EnvironmentError::NotFound(m) => Api::not_found(m),
        EnvironmentError::Conflict(m) => Api::conflict("duplicate", m),
        EnvironmentError::Invalid(m) => Api::unprocessable(m),
        EnvironmentError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/environments
pub async fn list_environments(
    State(state): State<AppState>,
) -> Result<Json<Vec<EnvironmentView>>, Api> {
    state.environments.list().await.map(Json).map_err(api_err)
}

/// POST /api/environments
pub async fn create_environment(
    State(state): State<AppState>,
    Json(input): Json<EnvironmentInput>,
) -> Result<(StatusCode, Json<EnvironmentView>), Api> {
    state
        .environments
        .create(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/environments/:name
pub async fn get_environment(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<EnvironmentView>, Api> {
    state.environments.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/environments/:name — full replace; the path is the id of record.
pub async fn replace_environment(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<EnvironmentInput>,
) -> Result<Json<EnvironmentView>, Api> {
    state
        .environments
        .replace(&name, input)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/environments/:name
///
/// Unconditional: nothing references an environment yet, so there is no
/// in-use guard like the agents one. When wiring arrives, revisit this.
pub async fn delete_environment(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .environments
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}
```

- [ ] **Step 2: Register module, state, routes**

In `server/src/http/mod.rs`:
1. Add `mod environments;` to the module list (after `mod config;` — keep it alphabetical: `admin, agents, auth, config, environments, error, github, ...`; note `pub mod error;` stays where it is).
2. Add to `AppState` after the `routines`/`routine_runner` block (or right after `agents` — match the surrounding doc-comment style):

```rust
    /// Named environments (experimental): CRUD over the definitions. Nothing
    /// consumes an environment yet.
    pub environments: Arc<crate::environments::EnvironmentService>,
```

3. Add routes after the `/api/agents/:name/invoke` route:

```rust
        .route(
            "/api/environments",
            get(environments::list_environments).post(environments::create_environment),
        )
        .route(
            "/api/environments/:name",
            get(environments::get_environment)
                .put(environments::replace_environment)
                .delete(environments::delete_environment),
        )
```

- [ ] **Step 3: Write the failing HTTP test**

In `server/src/http/mod.rs`'s `mod tests`, add to `test_state` (next to the `agents` construction):

```rust
        let environments = Arc::new(crate::environments::EnvironmentService::new(
            crate::environments::EnvironmentStore::new(opened.db.clone()),
        ));
```

and add `environments,` to the returned `AppState { ... }`.

Then add the test (mirrors `routines_crud_over_http`):

```rust
    #[tokio::test]
    async fn environments_crud_over_http() {
        use horsie_models::environments::EnvironmentView;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        let body = serde_json::json!({
            "name": "staging", "description": "fly box", "vendor": "fly",
            "repos": [{"url": "https://github.com/o/api", "gitRef": "dev"}],
            "envVars": [{"name": "RUST_LOG", "value": "debug"}],
            "provision": [{"name": "setup", "uses": "run", "with": [{"key": "cmd", "value": "make setup"}]}],
        });

        // Create -> 201 with the full view.
        let res = app
            .clone()
            .oneshot(post_json("/api/environments", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let v: EnvironmentView = read_json(res).await;
        assert_eq!(v.name, "staging");
        assert_eq!(v.vendor, "fly");
        assert_eq!(v.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(v.env_vars[0].name, "RUST_LOG");
        assert_eq!(v.provision[0].uses, "run");

        // Duplicate -> 409; bad slug, empty vendor, and "local" -> 422.
        let res = app
            .clone()
            .oneshot(post_json("/api/environments", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
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

        // List + get; unknown -> 404.
        let res = app.clone().oneshot(get("/api/environments")).await.unwrap();
        let list: Vec<EnvironmentView> = read_json(res).await;
        assert_eq!(list.len(), 1);
        let res = app
            .clone()
            .oneshot(get("/api/environments/staging"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(get("/api/environments/ghost"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Replace -> 200, full replace; rename via body -> 422.
        let upd = serde_json::json!({"name": "staging", "vendor": "docker"});
        let res = app
            .clone()
            .oneshot(put_json("/api/environments/staging", &upd))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: EnvironmentView = read_json(res).await;
        assert_eq!(v.vendor, "docker");
        assert!(v.repos.is_empty(), "PUT is a full replace");
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/environments/staging",
                &serde_json::json!({"name": "other", "vendor": "fly"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Delete -> 204; again -> 404.
        let res = app
            .clone()
            .oneshot(delete("/api/environments/staging"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .oneshot(delete("/api/environments/staging"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test --workspace environments_crud_over_http`
Expected: FAIL to compile — `main.rs` doesn't set `AppState.environments` yet (and verify `mod.rs` changes are in).

- [ ] **Step 5: Wire the binary**

In `server/src/bin/horsie-server/main.rs`, after the `routines` construction:

```rust
    let environments = Arc::new(horsie_server::environments::EnvironmentService::new(
        horsie_server::environments::EnvironmentStore::new(opened.db.clone()),
    ));
```

and add `environments,` to the `AppState { ... }` literal.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --workspace environments_crud_over_http`
Expected: PASS. Then run the whole http module tests: `cargo test --workspace http::` — Expected: PASS (the `AppState` change touches every test in the module).

- [ ] **Step 7: Commit**

```bash
git add server/src/http server/src/bin
git commit -m "feat(server): environments CRUD API"
```

---

### Task 6: Web API client + hooks

**Files:**
- Modify: `clients/web/src/api/client.ts` (add `environments` block; import the types)
- Create: `clients/web/src/hooks/useEnvironments.ts`

**Interfaces:**
- Consumes: Task 1's `EnvironmentView`/`EnvironmentInput` TS types; existing `request()` helper and `api` object shape in `client.ts`.
- Produces: `api.environments.{list,get,create,update,remove}`; hooks `useEnvironments()`, `useEnvironment(name)`, `useCreateEnvironment()`, `useUpdateEnvironment()`, `useDeleteEnvironment()` + `environmentKeys`.

- [ ] **Step 1: Add the API client block**

In `clients/web/src/api/client.ts`, check the type imports at the top of the file and add `EnvironmentInput`, `EnvironmentView` to the existing `import type { ... } from "./types"`. Then add after the `agents: { ... }` block (mirroring its doc comments):

```ts
  environments: {
    /** All environments. */
    list: (): Promise<EnvironmentView[]> => request("/environments"),

    get: (name: string): Promise<EnvironmentView> =>
      request(`/environments/${encodeURIComponent(name)}`),

    create: (body: EnvironmentInput): Promise<EnvironmentView> =>
      request("/environments", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. */
    update: (name: string, body: EnvironmentInput): Promise<EnvironmentView> =>
      request(`/environments/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/environments/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },
```

- [ ] **Step 2: Add the hooks**

`clients/web/src/hooks/useEnvironments.ts`:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { EnvironmentInput } from "../api/types";

export const environmentKeys = {
  all: ["environments"] as const,
  one: (name: string) => ["environments", name] as const,
};

/** All environments. */
export function useEnvironments() {
  return useQuery({
    queryKey: environmentKeys.all,
    queryFn: () => api.environments.list(),
  });
}

export function useEnvironment(name: string | undefined) {
  return useQuery({
    queryKey: name ? environmentKeys.one(name) : ["environments", "none"],
    queryFn: () => api.environments.get(name as string),
    enabled: !!name,
  });
}

export function useCreateEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: EnvironmentInput) => api.environments.create(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: environmentKeys.all }),
  });
}

export function useUpdateEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: EnvironmentInput }) =>
      api.environments.update(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: environmentKeys.all });
      qc.invalidateQueries({ queryKey: environmentKeys.one(name) });
    },
  });
}

export function useDeleteEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.environments.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: environmentKeys.all }),
  });
}
```

- [ ] **Step 3: Verify typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/api/client.ts clients/web/src/hooks/useEnvironments.ts
git commit -m "feat(web): environments api client and hooks"
```

---

### Task 7: Web pages, routes, sidebar

**Files:**
- Create: `clients/web/src/pages/environments/EnvironmentsPage.tsx`
- Create: `clients/web/src/pages/environments/EnvironmentsPage.test.tsx`
- Create: `clients/web/src/pages/environments/EnvironmentEditPage.tsx`
- Modify: `clients/web/src/App.tsx` (three routes)
- Modify: `clients/web/src/components/Sidebar.tsx` (PrimaryLink between Agents and Routines)

**Interfaces:**
- Consumes: Task 6's hooks; `EnvironmentView`, `EnvironmentInput`, `RepoConfig`, `EnvVar`, `ProvisionStep` from `api/types`; `RowLabel` from `../settings/fields`; `RailToggle` from `../../components/rail`; `ApiRequestError` from `../../api/client`.
- Produces: routes `/environments`, `/environments/new`, `/environments/:name/edit`; sidebar link `data-testid="environments-link"`.

- [ ] **Step 1: Write the failing list-page test**

`clients/web/src/pages/environments/EnvironmentsPage.test.tsx` (mirrors `AgentsPage.test.tsx`):

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EnvironmentView } from "../../api/types";
import { EnvironmentsPage } from "./EnvironmentsPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

const remove = vi.fn(async (_name: string) => {});
const list = vi.fn(async (): Promise<EnvironmentView[]> => environments);

vi.mock("../../api/client", () => ({
  api: {
    environments: {
      list: () => list(),
      remove: (name: string) => remove(name),
    },
  },
  ApiRequestError: class extends Error {},
}));

function env(name: string, over: Partial<EnvironmentView> = {}): EnvironmentView {
  return {
    name,
    description: `${name} description`,
    vendor: "fly",
    repos: [{ url: "https://github.com/o/api" }],
    envVars: [],
    provision: [],
    createdAt: "1",
    updatedAt: "1",
    ...over,
  };
}

const environments = [env("staging"), env("prod", { vendor: "docker" })];

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <EnvironmentsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("EnvironmentsPage", () => {
  it("renders one row per environment with its vendor and description", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("environment-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("staging");
    expect(rows[0].textContent).toContain("fly");
    expect(rows[0].textContent).toContain("staging description");
    expect(rows[0].textContent).toContain("1 repos");
    expect(rows[1].textContent).toContain("docker");
  });

  it("deletes the named environment once the confirm is accepted", async () => {
    const confirm = vi.spyOn(window, "confirm").mockImplementation(() => true);
    const { findByTestId } = renderPage();
    fireEvent.click(await findByTestId("delete-environment-prod"));
    await waitFor(() => expect(remove).toHaveBeenCalledWith("prod"));
    confirm.mockRestore();
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd clients/web && bun run test:unit -- environments`
Expected: FAIL (`EnvironmentsPage` doesn't exist).

- [ ] **Step 3: Implement the list page**

`clients/web/src/pages/environments/EnvironmentsPage.tsx` (structure mirrors `AgentsPage.tsx`):

```tsx
import { Plus, Trash2 } from "lucide-react";
import { RailToggle } from "../../components/rail";
import { Link, useNavigate } from "react-router-dom";
import { useEnvironments, useDeleteEnvironment } from "../../hooks/useEnvironments";

export function EnvironmentsPage() {
  const { data: environments, isLoading, isError } = useEnvironments();
  const del = useDeleteEnvironment();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="environments-page">
      <div className="flex items-center gap-2 border-b bg-panel px-4 py-3.5 sm:gap-3 sm:px-6">
        <RailToggle />
        <div className="min-w-0 flex-1">
          <h1 className="page-title">Environments</h1>
          <p className="mt-0.5 text-xs text-faint">
            Named runtime + repos bundles. Experimental — nothing uses them yet.
          </p>
        </div>
        <button
          className="key key-go shrink-0"
          onClick={() => navigate("/environments/new")}
          data-testid="new-environment-button"
        >
          <Plus size={13} aria-hidden />
          New environment
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          {isLoading && (
            <div className="flex items-center gap-2">
              <span className="lamp lamp-live text-amber-ink" aria-hidden />
              <span className="legend">Loading environments</span>
            </div>
          )}
          {isError && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              Can’t reach the server. Check that horsie-server is running, then
              reload.
            </p>
          )}
          {environments && environments.length === 0 && (
            <section className="panel p-4" data-testid="environments-empty">
              <h2 className="legend">Environment roster</h2>
              <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
                An environment is a saved runtime + repos bundle — where the
                work runs and what is checked out there. Press{" "}
                <span className="text-legend">New environment</span> to define
                one.
              </p>
            </section>
          )}
          <div className="space-y-2">
            {(environments ?? []).map((e) => (
              <div
                key={e.name}
                className="flex items-center gap-3 rounded-[var(--radius-control)] border bg-panel px-4 py-3 transition-colors hover:bg-raised"
                data-testid="environment-row"
                data-environment-name={e.name}
              >
                <Link
                  to={`/environments/${encodeURIComponent(e.name)}/edit`}
                  className="min-w-0 flex-1"
                >
                  <div className="flex items-baseline gap-2">
                    <span className="font-mono text-sm font-medium text-legend">
                      {e.name}
                    </span>
                    <span className="legend">{e.vendor}</span>
                  </div>
                  {e.description && (
                    <div className="truncate text-sm text-dim">{e.description}</div>
                  )}
                  <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-1">
                    {e.repos.length > 0 && (
                      <span className="legend">{e.repos.length} repos</span>
                    )}
                    {e.envVars.length > 0 && (
                      <span className="legend">{e.envVars.length} env</span>
                    )}
                    {e.provision.length > 0 && (
                      <span className="legend">{e.provision.length} steps</span>
                    )}
                  </div>
                </Link>
                <button
                  className="key-icon shrink-0 !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title={`Delete ${e.name}`}
                  data-testid={`delete-environment-${e.name}`}
                  onClick={() => {
                    if (window.confirm(`Delete environment '${e.name}'?`))
                      del.mutate(e.name);
                  }}
                >
                  <Trash2 size={15} />
                </button>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd clients/web && bun run test:unit -- environments`
Expected: PASS (2 tests).

- [ ] **Step 5: Implement the edit page**

`clients/web/src/pages/environments/EnvironmentEditPage.tsx`. Same mount-after-load pattern as `AgentEditPage`. Repos and env vars are editable rows; provision is a JSON textarea parsed on save.

```tsx
import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { Plus, Trash2 } from "lucide-react";
import { ApiRequestError } from "../../api/client";
import { RailToggle } from "../../components/rail";
import type { EnvironmentView, EnvVar, ProvisionStep, RepoConfig } from "../../api/types";
import {
  useCreateEnvironment,
  useEnvironment,
  useUpdateEnvironment,
} from "../../hooks/useEnvironments";
import { RowLabel } from "../settings/fields";

/** Create (`/environments/new`) and edit (`/environments/:name/edit`) share one
 * form, mounted only once the environment has loaded: the rows seed from
 * `initial` with `useState`, which cannot pick up a value that arrives later. */
export function EnvironmentEditPage() {
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useEnvironment(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">No such environment: {name}.</p>
    );
  }
  return <EnvironmentForm key={name ?? "new"} initial={existing} />;
}

function EnvironmentForm({ initial }: { initial?: EnvironmentView }) {
  const editing = !!initial;
  const create = useCreateEnvironment();
  const update = useUpdateEnvironment();
  const navigate = useNavigate();
  const [envName, setEnvName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [vendor, setVendor] = useState(initial?.vendor ?? "");
  const [repos, setRepos] = useState<RepoConfig[]>(initial?.repos ?? []);
  const [envVars, setEnvVars] = useState<EnvVar[]>(initial?.envVars ?? []);
  const [provisionText, setProvisionText] = useState(
    initial?.provision.length ? JSON.stringify(initial.provision, null, 2) : "",
  );
  const [error, setError] = useState<string | null>(null);
  const busy = create.isPending || update.isPending;
  const blockedReason =
    envName.trim() === ""
      ? "Give the environment a name to save it."
      : vendor.trim() === ""
        ? "Name the runtime vendor this environment runs on."
        : null;
  const canSave = !busy && blockedReason === null;

  const handleSave = async () => {
    setError(null);
    let provision: ProvisionStep[] | undefined;
    const text = provisionText.trim();
    if (text) {
      try {
        const parsed: unknown = JSON.parse(text);
        if (!Array.isArray(parsed)) throw new Error("not an array");
        provision = parsed as ProvisionStep[];
      } catch {
        setError("Provision steps must be a JSON array of {name, uses, with}.");
        return;
      }
    }
    const body = {
      name: envName.trim(),
      description: description.trim() || undefined,
      vendor: vendor.trim(),
      repos: repos.length ? repos : undefined,
      envVars: envVars.length ? envVars : undefined,
      provision,
    };
    try {
      if (editing) await update.mutateAsync({ name: envName.trim(), body });
      else await create.mutateAsync(body);
      navigate("/environments");
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to save environment.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="environment-edit-page">
      <header className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing ? `Edit ${initial.name}` : "New environment"}
        </h1>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto" data-popover-boundary>
        <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6 sm:px-6">
          <section className="panel space-y-4 p-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label className="block">
                <RowLabel>Name</RowLabel>
                <input
                  className="field field-mono"
                  placeholder="staging"
                  value={envName}
                  disabled={editing}
                  onChange={(e) => setEnvName(e.target.value)}
                  data-testid="environment-name-input"
                />
              </label>
              <label className="block">
                <RowLabel>Description</RowLabel>
                <input
                  className="field"
                  placeholder="What this environment is for"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  data-testid="environment-description-input"
                />
              </label>
            </div>
            <label className="block">
              <RowLabel>Runtime vendor</RowLabel>
              <input
                className="field field-mono"
                placeholder="fly"
                value={vendor}
                onChange={(e) => setVendor(e.target.value)}
                data-testid="environment-vendor-input"
              />
              <p className="mt-1 text-xs text-faint">
                The vendor that runs this environment. Local runtimes are not
                supported.
              </p>
            </label>
          </section>

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Repos</h2>
            {repos.map((r, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  className="field field-mono flex-1"
                  placeholder="https://github.com/org/repo"
                  value={r.url}
                  onChange={(e) =>
                    setRepos(repos.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))
                  }
                  data-testid={`repo-url-${i}`}
                />
                <input
                  className="field field-mono w-32"
                  placeholder="ref"
                  value={r.gitRef ?? ""}
                  onChange={(e) =>
                    setRepos(
                      repos.map((x, j) =>
                        j === i ? { ...x, gitRef: e.target.value || undefined } : x,
                      ),
                    )
                  }
                  data-testid={`repo-ref-${i}`}
                />
                <button
                  className="key-icon !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title="Remove repo"
                  data-testid={`repo-remove-${i}`}
                  onClick={() => setRepos(repos.filter((_, j) => j !== i))}
                >
                  <Trash2 size={15} />
                </button>
              </div>
            ))}
            <button
              className="key key-blank"
              onClick={() => setRepos([...repos, { url: "" }])}
              data-testid="repo-add"
            >
              <Plus size={13} aria-hidden />
              Add repo
            </button>
          </section>

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Env vars</h2>
            <p className="text-xs text-faint">
              Plain text only — no secrets here.
            </p>
            {envVars.map((v, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  className="field field-mono w-56"
                  placeholder="NAME"
                  value={v.name}
                  onChange={(e) =>
                    setEnvVars(envVars.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))
                  }
                  data-testid={`env-name-${i}`}
                />
                <input
                  className="field field-mono flex-1"
                  placeholder="value"
                  value={v.value}
                  onChange={(e) =>
                    setEnvVars(envVars.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))
                  }
                  data-testid={`env-value-${i}`}
                />
                <button
                  className="key-icon !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title="Remove env var"
                  data-testid={`env-remove-${i}`}
                  onClick={() => setEnvVars(envVars.filter((_, j) => j !== i))}
                >
                  <Trash2 size={15} />
                </button>
              </div>
            ))}
            <button
              className="key key-blank"
              onClick={() => setEnvVars([...envVars, { name: "", value: "" }])}
              data-testid="env-add"
            >
              <Plus size={13} aria-hidden />
              Add env var
            </button>
          </section>

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Provision steps</h2>
            <p className="text-xs text-faint">
              A JSON array of {"{name, uses, with}"} steps. Nothing runs them
              yet.
            </p>
            <textarea
              className="field field-mono min-h-28 w-full"
              placeholder='[{"name": "setup", "uses": "run", "with": [{"key": "cmd", "value": "make setup"}]}]'
              value={provisionText}
              onChange={(e) => setProvisionText(e.target.value)}
              data-testid="provision-input"
            />
          </section>

          {error && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              data-testid="environment-error"
            >
              {error}
            </div>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <button
              className="key key-go"
              disabled={!canSave}
              onClick={handleSave}
              data-testid="save-environment-button"
            >
              {busy ? "Saving…" : "Save environment"}
            </button>
            <button className="key key-blank" onClick={() => navigate("/environments")}>
              Cancel
            </button>
            {blockedReason && (
              <p className="text-xs leading-relaxed text-dim" data-testid="environment-blocked-hint">
                {blockedReason}
              </p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 6: Register routes and the sidebar link**

In `clients/web/src/App.tsx`, import the two pages and add after the agents routes:

```tsx
<Route path="environments" element={<EnvironmentsPage />} />
<Route path="environments/new" element={<EnvironmentEditPage />} />
<Route path="environments/:name/edit" element={<EnvironmentEditPage />} />
```

In `clients/web/src/components/Sidebar.tsx`, add `Container` to the `lucide-react` import and insert between the Agents and Routines `PrimaryLink`s:

```tsx
        <PrimaryLink
          to="/environments"
          testId="environments-link"
          icon={<Container size={15} aria-hidden />}
          label="Environments"
        />
```

- [ ] **Step 7: Verify web checks**

Run: `cd clients/web && bun run typecheck && bun run test:unit`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add clients/web/src
git commit -m "feat(web): environments pages and sidebar link"
```

---

### Task 8: Full verification + PR

- [ ] **Step 1: Run the full pre-PR gate**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
cd clients/web && bun run typecheck && bun run test:unit
```

Expected: all PASS. Fix anything that doesn't.

- [ ] **Step 2: Open the PR**

Push the branch and open a PR with a conventional title (`feat: environments — experimental runtime + repos bundles`) and a concise why/what body calling out: experimental, no wiring, vendor required/non-local, env vars plain-text, storage twins pattern. Then watch CI and fix any failures until green and mergeable.

## Self-Review Notes (completed by plan author)

- Spec coverage: schema (T1), migration (T2), store (T3), service (T4), http + wiring (T5), web client/hooks (T6), pages/routes/sidebar (T7), pre-PR checks + PR (T8). Out-of-scope items have no tasks, intentionally.
- Type consistency: `EnvironmentView`/`EnvironmentInput` (wire) and `EnvironmentRow`/twins (storage) names used identically across tasks; service methods match handler call sites; hook names match page imports.
- Known risk flagged inline: TS generation of cross-package imports (`executor` types) — Task 1 step 6 says how to resolve a collision.
