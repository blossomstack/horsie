# Agent Presets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Named, DB-backed agent presets on the server (CRUD + invoke-with-message → session), a CLI (`horsie agent list|get|invoke`, `horsie session list|status`), and a web UI Agents page.

**Architecture:** Server-side invoke: `POST /api/agents/:name/invoke` assembles a `SessionSpec` from the preset via a shared helper extracted from `create_session`, creates the session, queues the message, returns the session summary immediately. CLI is a thin reqwest REST client over existing fluorite wire types. Web UI adds `/agents` routes reusing the new-session pickers.

**Tech Stack:** Rust (axum, sqlx/SQLite, fluorite codegen, clap, reqwest), React 19 + react-query + Tailwind (clients/web), Playwright e2e.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-02-agent-presets-design.md`.
- Work in the worktree `.horsie/worktrees/agents`, branch `feat/agent-presets`.
- Protocol types are fluorite-generated (`models/fluorite/*.fl`); persisted row types are hand-written in the storage layer — never conflate.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`; test modules opt out with `#[cfg(test)] #[allow(...)]` (match existing file headers).
- All server errors use the `ApiError` envelope via `server/src/http/error.rs` helpers.
- Agent names are slugs: reuse `crate::memory::validate_slug`.
- Gates before PR: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace` (never `-p` single-crate), `cd clients/web && bun run typecheck && bun run test:unit`.
- TS types regenerate via `bun run generate-types` (needs `fluorite` CLI on PATH — present).

---

### Task 1: Wire types — `models/fluorite/agents.fl` + models registration + TS regen

**Files:**
- Create: `models/fluorite/agents.fl`
- Modify: `models/src/lib.rs` (add `pub mod agents`)
- Modify: `clients/web/package.json` (generate-types script), `clients/ts/package.json` (same)
- Modify: `clients/web/src/api/types.ts` (export generated agents module)

**Interfaces:**
- Produces: `horsie_models::agents::{AgentView, AgentInput, AgentInvokeRequest, AgentInvokeResponse}`; TS `AgentView`/`AgentInput` etc. from `../generated/agents`.

- [ ] **Step 1: Write `models/fluorite/agents.fl`**

```fluorite
/// Named agent presets: a saved session configuration (runtime vendor, model,
/// repos, skills, MCP servers, memory spaces, thinking effort) that is invoked
/// with a message to create a session.
package agents;

use session.SessionSummary;
use session_api.RepoConfig;

/// An agent preset as shown to clients.
struct AgentView {
    /// Slug; the id of record, used in API paths and CLI invocations.
    name: String,
    description: String,
    /// Runtime vendor name; absent → the server's default vendor at invoke.
    vendor: Option<String>,
    /// Configured model alias.
    model: String,
    /// Repositories cloned into the session workspace at provision time.
    repos: Vec<RepoConfig>,
    /// Selected plugin-bundle (skill) names.
    plugins: Vec<String>,
    /// Enabled MCP server names.
    mcp_servers: Vec<String>,
    /// Memory spaces the session may read and write.
    memory_spaces: Vec<String>,
    /// Canonical thinking effort; absent → the model's configured default.
    thinking_effort: Option<String>,
    /// Unix epoch seconds.
    created_at: String,
    updated_at: String,
}

/// Create or fully replace an agent preset. Omitted list fields default to
/// empty; `description` defaults to "".
struct AgentInput {
    name: String,
    description: Option<String>,
    vendor: Option<String>,
    model: String,
    repos: Option<Vec<RepoConfig>>,
    plugins: Option<Vec<String>>,
    mcp_servers: Option<Vec<String>>,
    memory_spaces: Option<Vec<String>>,
    thinking_effort: Option<String>,
}

struct AgentInvokeRequest {
    /// First user message; queued immediately after the session is created.
    message: String,
    /// Optional session title.
    name: Option<String>,
}

struct AgentInvokeResponse { session: SessionSummary }
```

- [ ] **Step 2: Register the module in `models/src/lib.rs`**

Add after the `plugins` module block:

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agents {
    include!(concat!(env!("OUT_DIR"), "/agents/mod.rs"));
}
```

- [ ] **Step 3: Add a serde round-trip test pinning the wire shape**

Append to the `tests` mod at the bottom of `models/src/lib.rs`:

```rust
    #[test]
    fn agent_view_round_trips_with_camel_case_keys() {
        use crate::agents::AgentView;
        let view = AgentView {
            name: "reviewer".into(),
            description: "reviews PRs".into(),
            vendor: Some("local".into()),
            model: "sonnet".into(),
            repos: vec![],
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: Some("high".into()),
            created_at: "1".into(),
            updated_at: "2".into(),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"mcpServers\""), "{json}");
        assert!(json.contains("\"thinkingEffort\":\"high\""), "{json}");
        let back: AgentView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
    }
```

Note: the existing `tests` mod imports `super::capabilities` and `super::session`; add `use super::agents;` isn't needed if referencing `crate::agents::AgentView` inside the fn as above.

- [ ] **Step 4: Run models tests**

Run: `cargo test --workspace -- horsie_models` (or full `cargo test --workspace` — the workspace is the supported invocation)
Expected: PASS, `agents` module compiles and the round-trip test passes.

- [ ] **Step 5: Regenerate TS types**

In `clients/web/package.json` and `clients/ts/package.json`, append `../../models/fluorite/agents.fl` to the `-i` list of the `generate-types` script (keep the existing order, add agents last).

Add to `clients/web/src/api/types.ts`:

```ts
export * from "../generated/agents";
```

Run:
```bash
cd clients/web && bun run generate-types
cd ../ts && npm run generate-types
```
Expected: `clients/web/src/generated/agents/` and `clients/ts/src/generated/agents/` exist with `agentView.ts`, `agentInput.ts`, `agentInvokeRequest.ts`, `agentInvokeResponse.ts`, and an `index.ts` (or equivalent per the generator's layout — mirror what `memory.fl` produced).

- [ ] **Step 6: Commit**

```bash
git add models/fluorite/agents.fl models/src/lib.rs clients/web/package.json clients/ts/package.json clients/web/src/api/types.ts clients/web/src/generated clients/ts/src/generated
git commit -m "feat(models): agent preset wire types (agents.fl) + TS regen"
```

---

### Task 2: Migration + AgentStore

**Files:**
- Create: `server/migrations/0014_agents.sql`
- Create: `server/src/agents/mod.rs`
- Create: `server/src/agents/store.rs`
- Modify: `server/src/lib.rs` (add `pub mod agents;`)

**Interfaces:**
- Produces: `AgentStore::{new, list, get, insert, replace, delete}`, `AgentRow`, `AgentRepo`. `AgentRepo` is the storage twin of wire `RepoConfig` (hand-written, serde-JSON column).

- [ ] **Step 1: Write the migration**

`server/migrations/0014_agents.sql`:

```sql
-- Named agent presets: a saved session configuration (vendor, model, repos,
-- skills, MCP servers, memory spaces, thinking effort) invoked with a message
-- to create a session. List-typed columns are JSON arrays; `repos` elements
-- are {"url", "git_ref"?, "dir"?}.

CREATE TABLE agents (
    name            TEXT PRIMARY KEY,
    description     TEXT NOT NULL DEFAULT '',
    vendor          TEXT,                       -- NULL → server default at invoke
    model           TEXT NOT NULL,
    repos           TEXT NOT NULL DEFAULT '[]',
    plugins         TEXT NOT NULL DEFAULT '[]',
    mcp_servers     TEXT NOT NULL DEFAULT '[]',
    memory_spaces   TEXT NOT NULL DEFAULT '[]',
    thinking_effort TEXT,
    created_at      TEXT NOT NULL,              -- unix epoch seconds
    updated_at      TEXT NOT NULL               -- unix epoch seconds
);
```

- [ ] **Step 2: Write `server/src/agents/mod.rs`**

```rust
//! Named agent presets: a saved session configuration invoked with a message
//! to create a session. Mirrors the `memory` module's store/service split and
//! shares the config store's SqlitePool. Row types are hand-written storage
//! types; the fluorite wire types in `horsie_models::agents` are mapped at the
//! service boundary.

mod service;
mod store;

pub use service::{AgentError, AgentService};
pub use store::{AgentRepo, AgentRow, AgentStore};
```

Add `pub mod agents;` to `server/src/lib.rs` (alphabetical, before `config`).

- [ ] **Step 3: Write the failing store tests**

`server/src/agents/store.rs` — full file, tests included (TDD: write the test mod first, watch it fail to compile, then implement):

```rust
//! SQLite storage for agent presets, sharing the config store's pool.
//! List-typed columns are JSON; `AgentRepo` is the storage twin of the wire
//! `session_api::RepoConfig` (protocol types are not storage types).

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

const COLS: &str = "name, description, vendor, model, repos, plugins, \
                    mcp_servers, memory_spaces, thinking_effort, created_at, updated_at";

/// One repo to clone at provision time (storage twin of wire `RepoConfig`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentRepo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// One row of the `agents` table.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    pub vendor: Option<String>,
    pub model: String,
    pub repos: Vec<AgentRepo>,
    pub plugins: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub memory_spaces: Vec<String>,
    pub thinking_effort: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AgentStore {
    pool: SqlitePool,
}

impl AgentStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list(&self) -> Result<Vec<AgentRow>, String> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM agents ORDER BY name"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_agent).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<AgentRow>, String> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM agents WHERE name = ?"))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_agent).transpose()
    }

    /// Insert; errs when the name is taken (no upsert — a silent overwrite
    /// would discard the existing preset).
    pub async fn insert(&self, row: &AgentRow) -> Result<(), String> {
        sqlx::query(&format!(
            "INSERT INTO agents ({COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(&row.model)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create agent '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no agent has that name.
    pub async fn replace(&self, row: &AgentRow) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE agents SET description = ?, vendor = ?, model = ?, repos = ?, \
             plugins = ?, mcp_servers = ?, memory_spaces = ?, thinking_effort = ?, \
             updated_at = ? WHERE name = ?",
        )
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(&row.model)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.updated_at)
        .bind(&row.name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query("DELETE FROM agents WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

fn from_json<T: serde::de::DeserializeOwned>(col: &str, text: String) -> Result<T, String> {
    serde_json::from_str(&text).map_err(|e| format!("agents.{col}: {e}"))
}

fn row_to_agent(row: &SqliteRow) -> Result<AgentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_opt = |c: &str| row.try_get::<Option<String>, _>(c).map_err(|e| e.to_string());
    Ok(AgentRow {
        name: get("name")?,
        description: get("description")?,
        vendor: get_opt("vendor")?,
        model: get("model")?,
        repos: from_json("repos", get("repos")?)?,
        plugins: from_json("plugins", get("plugins")?)?,
        mcp_servers: from_json("mcp_servers", get("mcp_servers")?)?,
        memory_spaces: from_json("memory_spaces", get("memory_spaces")?)?,
        thinking_effort: get_opt("thinking_effort")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn store() -> (AgentStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (AgentStore::new(pool), tmp)
    }

    fn row(name: &str) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: "d".into(),
            vendor: Some("local".into()),
            model: "sonnet".into(),
            repos: vec![AgentRepo {
                url: "https://github.com/o/api".into(),
                git_ref: Some("dev".into()),
                dir: None,
            }],
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: Some("high".into()),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_json_columns() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got, row("a"));
        assert_eq!(got.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.insert(&row("a")).await.is_err());
    }

    #[tokio::test]
    async fn replace_updates_and_reports_misses() {
        let (s, _t) = store().await;
        assert!(!s.replace(&row("ghost")).await.unwrap());
        s.insert(&row("a")).await.unwrap();
        let mut r = row("a");
        r.description = "new".into();
        r.updated_at = "2".into();
        assert!(s.replace(&r).await.unwrap());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.description, "new");
        assert_eq!(got.updated_at, "2");
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn null_vendor_and_effort_round_trip() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.vendor = None;
        r.thinking_effort = None;
        s.insert(&r).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.vendor, None);
        assert_eq!(got.thinking_effort, None);
    }
}
```

- [ ] **Step 4: Run the store tests**

Run: `cargo test --workspace -- agents::store`
Expected: PASS (migration picked up by `sqlx::migrate!()`).

- [ ] **Step 5: Commit**

```bash
git add server/migrations/0014_agents.sql server/src/agents/mod.rs server/src/agents/store.rs server/src/lib.rs
git commit -m "feat(server): agents table migration + AgentStore"
```

---

### Task 3: AgentService + AgentError + save-time validation

**Files:**
- Create: `server/src/agents/service.rs`

**Interfaces:**
- Consumes: `AgentStore`/`AgentRow`/`AgentRepo` (Task 2), `crate::memory::validate_slug`, `crate::config::ConfigStore` (`view()`, returns `SettingsView` with `models: Vec<ModelView>`), wire types from Task 1.
- Produces:
  ```rust
  pub enum AgentError { NotFound(String), Conflict(String), Invalid(String), Internal(String) }
  pub struct AgentService { ... }
  impl AgentService {
      pub fn new(store: AgentStore, config: Arc<dyn ConfigStore>) -> Self;
      pub async fn list(&self) -> Result<Vec<AgentView>, AgentError>;
      pub async fn get(&self, name: &str) -> Result<AgentView, AgentError>;
      pub async fn create(&self, input: AgentInput) -> Result<AgentView, AgentError>;
      pub async fn replace(&self, name: &str, input: AgentInput) -> Result<AgentView, AgentError>;
      pub async fn delete(&self, name: &str) -> Result<(), AgentError>;
  }
  ```

- [ ] **Step 1: Write `server/src/agents/service.rs` (tests first, then implementation)**

```rust
//! Validation, timestamps, and row↔wire mapping over `AgentStore`. Save-time
//! validation covers only what's stable at save: the name slug, the model
//! alias, and the thinking effort the model offers. Vendors, plugins, MCP
//! servers, and memory spaces are live/external rosters — validated at invoke.

use crate::agents::store::{AgentRepo, AgentRow, AgentStore};
use crate::config::ConfigStore;
use horsie_models::agents::{AgentInput, AgentView};
use horsie_models::session_api::RepoConfig;
use std::sync::Arc;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum AgentError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

pub struct AgentService {
    store: AgentStore,
    config: Arc<dyn ConfigStore>,
}

impl AgentService {
    pub fn new(store: AgentStore, config: Arc<dyn ConfigStore>) -> Self {
        Self { store, config }
    }

    pub async fn list(&self) -> Result<Vec<AgentView>, AgentError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(AgentError::Internal)?
            .iter()
            .map(agent_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<AgentView, AgentError> {
        self.store
            .get(name)
            .await
            .map_err(AgentError::Internal)?
            .as_ref()
            .map(agent_view)
            .ok_or_else(|| AgentError::NotFound(format!("unknown agent '{name}'")))
    }

    pub async fn create(&self, input: AgentInput) -> Result<AgentView, AgentError> {
        self.validate(&input).await?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(AgentError::Internal)?
            .is_some()
        {
            return Err(AgentError::Conflict(format!(
                "agent '{}' already exists",
                input.name
            )));
        }
        let now = now_secs();
        let row = row_from_input(input, now.clone(), now);
        self.store.insert(&row).await.map_err(AgentError::Internal)?;
        self.get(&row.name).await
    }

    /// Full replace. The path name is the id of record: a body naming a
    /// different agent is a 422, matching the MCP upsert convention.
    pub async fn replace(&self, name: &str, input: AgentInput) -> Result<AgentView, AgentError> {
        if input.name != name {
            return Err(AgentError::Invalid(
                "agent name is immutable; the path is the id of record".to_string(),
            ));
        }
        let existing = self
            .store
            .get(name)
            .await
            .map_err(AgentError::Internal)?
            .ok_or_else(|| AgentError::NotFound(format!("unknown agent '{name}'")))?;
        self.validate(&input).await?;
        let row = row_from_input(input, existing.created_at, now_secs());
        self.store
            .replace(&row)
            .await
            .map_err(AgentError::Internal)?;
        self.get(name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), AgentError> {
        if self.store.delete(name).await.map_err(AgentError::Internal)? {
            Ok(())
        } else {
            Err(AgentError::NotFound(format!("unknown agent '{name}'")))
        }
    }

    /// Save-time validation: slug, configured model, offered thinking effort.
    async fn validate(&self, input: &AgentInput) -> Result<(), AgentError> {
        crate::memory::validate_slug(&input.name).map_err(AgentError::Invalid)?;
        let view = self.config.view().await.map_err(AgentError::Internal)?;
        let model = view
            .models
            .iter()
            .find(|m| m.alias == input.model)
            .ok_or_else(|| {
                AgentError::Invalid(format!("unknown model '{}'", input.model))
            })?;
        if let Some(effort) = input.thinking_effort.as_deref() {
            let offered = model.thinking_efforts.clone().unwrap_or_default();
            if !offered.iter().any(|e| e == effort) {
                return Err(AgentError::Invalid(format!(
                    "model '{}' does not offer thinking effort '{effort}'",
                    input.model
                )));
            }
        }
        Ok(())
    }
}

fn row_from_input(input: AgentInput, created_at: String, updated_at: String) -> AgentRow {
    AgentRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        vendor: input.vendor.filter(|v| !v.trim().is_empty()),
        model: input.model,
        repos: input
            .repos
            .unwrap_or_default()
            .into_iter()
            .map(|r| AgentRepo {
                url: r.url,
                git_ref: r.git_ref,
                dir: r.dir,
            })
            .collect(),
        plugins: input.plugins.unwrap_or_default(),
        mcp_servers: input.mcp_servers.unwrap_or_default(),
        memory_spaces: input.memory_spaces.unwrap_or_default(),
        thinking_effort: input.thinking_effort,
        created_at,
        updated_at,
    }
}

fn agent_view(row: &AgentRow) -> AgentView {
    AgentView {
        name: row.name.clone(),
        description: row.description.clone(),
        vendor: row.vendor.clone(),
        model: row.model.clone(),
        repos: row
            .repos
            .iter()
            .map(|r| RepoConfig {
                url: r.url.clone(),
                git_ref: r.git_ref.clone(),
                dir: r.dir.clone(),
            })
            .collect(),
        plugins: row.plugins.clone(),
        mcp_servers: row.mcp_servers.clone(),
        memory_spaces: row.memory_spaces.clone(),
        thinking_effort: row.thinking_effort.clone(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::settings::{ModelInput, ProviderInput, SettingsUpdate};

    /// A service on a temp DB with one provider ("p") and two models:
    /// "sonnet" (offers thinking efforts) and "haiku" (none).
    async fn service() -> (AgentService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("config.db");
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                info: horsie_models::settings::ServerInfo {
                    config_path: String::new(),
                    database: String::new(),
                    state_dir: String::new(),
                    data_dir: String::new(),
                    plugins_dir: String::new(),
                    version: "test".into(),
                },
            },
        )
        .await
        .unwrap();
        opened
            .store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "p".into(),
                    kind: "anthropic".into(),
                    base_url: Some("http://localhost:1".into()),
                    api_key: Some("sk-x".into()),
                    keep_thinking_signature: None,
                }]),
                models: Some(vec![
                    ModelInput {
                        alias: "sonnet".into(),
                        provider: "p".into(),
                        model_id: "claude-sonnet-4-6".into(),
                        max_tokens: None,
                        context_window: None,
                        thinking_efforts: Some(vec!["low".into(), "high".into()]),
                        thinking_effort: None,
                        thinking_dialect: None,
                        forced_tools_disable_thinking: None,
                    },
                    ModelInput {
                        alias: "haiku".into(),
                        provider: "p".into(),
                        model_id: "claude-haiku-4-5".into(),
                        max_tokens: None,
                        context_window: None,
                        thinking_efforts: None,
                        thinking_effort: None,
                        thinking_dialect: None,
                        forced_tools_disable_thinking: None,
                    },
                ]),
                default_vendor: None,
            })
            .await
            .unwrap();
        (
            AgentService::new(
                AgentStore::new(opened.pool.clone()),
                opened.store.clone(),
            ),
            tmp,
        )
    }

    fn input(name: &str, model: &str) -> AgentInput {
        AgentInput {
            name: name.into(),
            description: Some("d".into()),
            vendor: None,
            model: model.into(),
            repos: None,
            plugins: None,
            mcp_servers: None,
            memory_spaces: None,
            thinking_effort: None,
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_defaults_and_timestamps() {
        let (s, _t) = service().await;
        let v = s.create(input("a", "sonnet")).await.unwrap();
        assert_eq!(v.name, "a");
        assert_eq!(v.description, "d");
        assert_eq!(v.vendor, None);
        assert!(v.repos.is_empty() && v.plugins.is_empty());
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn create_validates_slug_model_and_thinking_effort() {
        let (s, _t) = service().await;
        let mut bad = input("Not A Slug", "sonnet");
        assert!(matches!(
            s.create(bad.clone()).await.unwrap_err(),
            AgentError::Invalid(_)
        ));
        bad = input("a", "ghost-model");
        let err = s.create(bad.clone()).await.unwrap_err();
        assert!(matches!(err, AgentError::Invalid(m) if m.contains("ghost-model")));
        bad = input("a", "haiku");
        bad.thinking_effort = Some("high".into());
        let err = s.create(bad).await.unwrap_err();
        assert!(matches!(err, AgentError::Invalid(m) if m.contains("haiku")));
        // An offered effort passes.
        let mut ok = input("a", "sonnet");
        ok.thinking_effort = Some("high".into());
        assert!(s.create(ok).await.is_ok());
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let (s, _t) = service().await;
        s.create(input("a", "sonnet")).await.unwrap();
        assert!(matches!(
            s.create(input("a", "sonnet")).await.unwrap_err(),
            AgentError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn replace_swaps_fields_and_keeps_created_at() {
        let (s, _t) = service().await;
        let v = s.create(input("a", "sonnet")).await.unwrap();
        let mut upd = input("a", "haiku");
        upd.description = Some("new".into());
        let got = s.replace("a", upd).await.unwrap();
        assert_eq!(got.model, "haiku");
        assert_eq!(got.description, "new");
        assert_eq!(got.created_at, v.created_at);
        // Rename via body → invalid; unknown → not found.
        assert!(matches!(
            s.replace("a", input("b", "sonnet")).await.unwrap_err(),
            AgentError::Invalid(_)
        ));
        assert!(matches!(
            s.replace("ghost", input("ghost", "sonnet")).await.unwrap_err(),
            AgentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn delete_and_get_report_unknown_names() {
        let (s, _t) = service().await;
        assert!(matches!(
            s.get("ghost").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
        assert!(matches!(
            s.delete("ghost").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
        s.create(input("a", "sonnet")).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(matches!(
            s.get("a").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_is_ordered_by_name() {
        let (s, _t) = service().await;
        s.create(input("b", "sonnet")).await.unwrap();
        s.create(input("a", "haiku")).await.unwrap();
        let names: Vec<String> = s.list().await.unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
```

Note: `ModelInput` field set must match `models/fluorite/settings.fl` exactly (alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect, forced_tools_disable_thinking) — verify against the generated struct while implementing; `ProviderInput` likewise (name, kind, base_url, api_key, keep_thinking_signature).

- [ ] **Step 2: Run the service tests**

Run: `cargo test --workspace -- agents::service`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add server/src/agents/service.rs server/src/agents/mod.rs
git commit -m "feat(server): AgentService with save-time validation"
```

---

### Task 4: Extract shared `build_session_spec` from `create_session`

**Files:**
- Modify: `server/src/http/handlers.rs`

**Interfaces:**
- Produces (all `pub(crate)`, for Task 5):
  ```rust
  pub(crate) async fn ask<T, F>(state: &AppState, make: F) -> Result<T, Api>
  pub(crate) fn summary(id: &str, rec: &SessionRecord, status: Option<&SessionStatus>) -> SessionSummary
  pub(crate) fn now_ms() -> u64
  pub(crate) async fn build_session_spec(
      state: &AppState,
      name: Option<String>,
      agent: WireAgentSettings,
      vendor: Option<String>,
      repos: Vec<RepoConfig>,
      plugins: Option<Vec<String>>,
      capabilities: Option<CapabilitySpec>,
  ) -> Result<SessionSpec, Api>
  ```
  (`RepoConfig` = `horsie_models::session_api::RepoConfig`; `CapabilitySpec` = `horsie_models::capabilities::CapabilitySpec`.)

- [ ] **Step 1: Refactor — move the spec-assembly body out of `create_session`**

Behavior-preserving: `create_session` becomes

```rust
pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    let spec = build_session_spec(
        &state,
        req.name,
        req.agent,
        req.vendor,
        req.repos.unwrap_or_default(),
        req.plugins,
        req.capabilities,
    )
    .await?;
    let created_at = now_ms();
    let id = ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    let rec = SessionRecord { spec, created_at };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session: summary(&id, &rec, Some(&SessionStatus::Idle)),
        }),
    ))
}
```

`build_session_spec` contains the moved logic verbatim (repos → provision steps, caps defaulting incl. the network-allow override when provision steps exist and no explicit caps, plugins imply `use_plugins`, thinking-effort resolution/validation against the config view), ending with:

```rust
    Ok(SessionSpec {
        name,
        agent,
        workspaces: vec![WorkspaceDef { name: "main".into() }],
        provision,
        vendor: vendor.unwrap_or_else(|| state.config_store.default_vendor()),
        plugins,
    })
```

Change `ask`, `summary`, `now_ms` from private to `pub(crate)`. Add `RepoConfig` to the `horsie_models::session_api` import list.

- [ ] **Step 2: Run the existing session HTTP tests**

Run: `cargo test --workspace -- http::`
Expected: PASS — no behavior change; existing `create_list_get_message_lifecycle_over_http`, `create_with_repos_builds_provision_steps`, and thinking-effort tests guard the refactor.

- [ ] **Step 3: Commit**

```bash
git add server/src/http/handlers.rs
git commit -m "refactor(server): extract shared build_session_spec from create_session"
```

---

### Task 5: HTTP agents resource + invoke endpoint

**Files:**
- Create: `server/src/http/agents.rs`
- Modify: `server/src/http/mod.rs` (routes, `mod agents;`, `AppState.agents`, test_state wiring)
- Modify: `server/src/bin/horsie-server/main.rs` (construct AgentService)

**Interfaces:**
- Consumes: `AgentService`/`AgentError` (Task 3), `handlers::{ask, summary, now_ms, build_session_spec}` (Task 4), `state.vendor_agents.connected_names()`.
- Produces: routes `GET/POST /api/agents`, `GET/PUT/DELETE /api/agents/:name`, `POST /api/agents/:name/invoke`.

- [ ] **Step 1: Write `server/src/http/agents.rs`**

```rust
//! HTTP surface for agent presets: CRUD for the web UI and CLI, plus
//! `POST /api/agents/:name/invoke` — create a session from the preset and
//! queue the first message in one call, returning the session immediately.

use super::AppState;
use super::error::Api;
use super::handlers;
use crate::agents::AgentError;
use crate::sessions::UserMessageError;
use crate::sessions::spec::SessionStatus;
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::agents::{AgentInput, AgentInvokeRequest, AgentInvokeResponse, AgentView};
use horsie_models::session::AgentSettings as WireAgentSettings;

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: AgentError) -> Api {
    match e {
        AgentError::NotFound(m) => Api::not_found(m),
        AgentError::Conflict(m) => Api::conflict("duplicate", m),
        AgentError::Invalid(m) => Api::unprocessable(m),
        AgentError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/agents
pub async fn list_agents(State(state): State<AppState>) -> Result<Json<Vec<AgentView>>, Api> {
    state.agents.list().await.map(Json).map_err(api_err)
}

/// POST /api/agents
pub async fn create_agent(
    State(state): State<AppState>,
    Json(input): Json<AgentInput>,
) -> Result<(StatusCode, Json<AgentView>), Api> {
    state
        .agents
        .create(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/agents/:name
pub async fn get_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<AgentView>, Api> {
    state.agents.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/agents/:name — full replace; the path is the id of record.
pub async fn replace_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<AgentInput>,
) -> Result<Json<AgentView>, Api> {
    state
        .agents
        .replace(&name, input)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/agents/:name
pub async fn delete_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .agents
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

/// POST /api/agents/:name/invoke — create a session from the preset and queue
/// the message; returns as soon as both are accepted (the turn runs in the
/// background).
pub async fn invoke_agent(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<AgentInvokeRequest>,
) -> Result<(StatusCode, Json<AgentInvokeResponse>), Api> {
    let agent = state.agents.get(&name).await.map_err(api_err)?;
    if req.message.trim().is_empty() {
        return Err(Api::unprocessable("message must not be empty"));
    }
    let vendor = agent
        .vendor
        .clone()
        .unwrap_or_else(|| state.config_store.default_vendor());
    if !state.vendor_agents.connected_names().contains(&vendor) {
        return Err(Api::unprocessable(format!(
            "runtime vendor '{vendor}' is not connected"
        )));
    }
    // The preset validated its model at save, but models are editable
    // settings — re-check so a stale preset fails here, not as a turn error.
    let view = state.config_store.view().await.map_err(Api::internal)?;
    if !view.models.iter().any(|m| m.alias == agent.model) {
        return Err(Api::unprocessable(format!(
            "model '{}' is no longer configured",
            agent.model
        )));
    }
    let wire = WireAgentSettings {
        model: agent.model.clone(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: None,
        mcp_servers: Some(agent.mcp_servers.clone()),
        memory_spaces: Some(agent.memory_spaces.clone()),
        thinking_effort: agent.thinking_effort.clone(),
        max_concurrent_subagents: None,
    };
    let spec = handlers::build_session_spec(
        &state,
        req.name,
        wire,
        Some(vendor),
        agent.repos.clone(),
        Some(agent.plugins.clone()),
        None,
    )
    .await?;
    let created_at = handlers::now_ms();
    let id = handlers::ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    handlers::ask(&state, |reply| SessionSupervisorCommand::UserMessage {
        id: id.clone(),
        text: req.message,
        reply,
    })
    .await?
    .map_err(|e| match e {
        UserMessageError::NotFound => Api::not_found("no such session"),
        UserMessageError::Unrecoverable(reason) => Api::conflict("unrecoverable", reason),
    })?;
    let rec = SessionRecord { spec, created_at };
    Ok((
        StatusCode::CREATED,
        Json(AgentInvokeResponse {
            session: handlers::summary(&id, &rec, Some(&SessionStatus::Idle)),
        }),
    ))
}
```

- [ ] **Step 2: Wire routes + AppState**

In `server/src/http/mod.rs`:
- Add `mod agents;` (alphabetical, after `mod admin;`… keep existing order style).
- Add to `AppState`:
  ```rust
  /// Named agent presets: CRUD plus invoke-with-message.
  pub agents: Arc<crate::agents::AgentService>,
  ```
- Add routes after the sessions routes:
  ```rust
  .route(
      "/api/agents",
      get(agents::list_agents).post(agents::create_agent),
  )
  .route(
      "/api/agents/:name",
      get(agents::get_agent)
          .put(agents::replace_agent)
          .delete(agents::delete_agent),
  )
  .route("/api/agents/:name/invoke", post(agents::invoke_agent))
  ```
- In `test_state`, after `let memory = ...`:
  ```rust
  let agents = Arc::new(crate::agents::AgentService::new(
      crate::agents::AgentStore::new(opened.pool.clone()),
      opened.store.clone(),
  ));
  ```
  and add `agents,` to the `AppState { ... }` literal.

In `server/src/bin/horsie-server/main.rs`, after the `let memory = ...` block:
```rust
    let agents = Arc::new(horsie_server::agents::AgentService::new(
        horsie_server::agents::AgentStore::new(opened.pool.clone()),
        opened.store.clone(),
    ));
```
and add `agents,` to the `AppState { ... }` literal.

- [ ] **Step 3: Write the failing HTTP tests**

Append to the `tests` mod in `server/src/http/mod.rs`:

```rust
    /// PUT a provider + model so agent save-time validation has a model to
    /// reference; returns the model alias ("mock").
    async fn put_mock_model(app: &axum::Router) {
        let body = serde_json::json!({
            "providers": [{"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}],
            "models": [{"alias": "mock", "provider": "p", "modelId": "id"}],
        });
        let res = app
            .clone()
            .oneshot(put_json("/api/config", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn agents_crud_over_http() {
        use horsie_models::agents::AgentView;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        put_mock_model(&app).await;

        // Create → 201 with the stored view.
        let body = serde_json::json!({
            "name": "reviewer", "description": "reviews PRs", "model": "mock",
            "vendor": "mock", "plugins": ["superpowers"], "memorySpaces": ["default"]
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/agents", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let v: AgentView = read_json(res).await;
        assert_eq!(v.name, "reviewer");
        assert_eq!(v.vendor.as_deref(), Some("mock"));

        // Duplicate → 409; bad slug → 422; unknown model → 422.
        let res = app
            .clone()
            .oneshot(post_json("/api/agents", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents",
                &serde_json::json!({"name": "Bad Name", "model": "mock"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents",
                &serde_json::json!({"name": "x", "model": "ghost"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // List + get.
        let res = app.clone().oneshot(get("/api/agents")).await.unwrap();
        let list: Vec<AgentView> = read_json(res).await;
        assert_eq!(list.len(), 1);
        let res = app
            .clone()
            .oneshot(get("/api/agents/reviewer"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Replace → 200; unknown replace → 404; name mismatch → 422.
        let upd = serde_json::json!({"name": "reviewer", "model": "mock", "description": "v2"});
        let res = app
            .clone()
            .oneshot(put_json("/api/agents/reviewer", &upd))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: AgentView = read_json(res).await;
        assert_eq!(v.description, "v2");
        assert!(v.plugins.is_empty(), "PUT is a full replace");
        let res = app
            .clone()
            .oneshot(put_json("/api/agents/ghost", &upd))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/agents/reviewer",
                &serde_json::json!({"name": "other", "model": "mock"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Delete → 204; again → 404.
        let res = app
            .clone()
            .oneshot(delete("/api/agents/reviewer"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .oneshot(delete("/api/agents/reviewer"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invoke_creates_a_session_and_queues_the_message() {
        use horsie_models::agents::{AgentInvokeResponse, AgentView};
        use horsie_models::session_api::{GetSessionResponse, ListSessionsResponse};
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        put_mock_model(&app).await;
        let body = serde_json::json!({
            "name": "reviewer", "model": "mock", "vendor": "mock",
            "memorySpaces": ["default"]
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/agents", &body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        // Invoke → 201 with the session id, immediately.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/reviewer/invoke",
                &serde_json::json!({"message": "review the diff"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let invoked: AgentInvokeResponse = read_json(res).await;
        let id = invoked.session.id;

        // The session exists with the preset's model/vendor, and the message
        // is queued in its inbox (journaled, not yet answered).
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
        assert_eq!(detail.session.memory_spaces, vec!["default"]);
        assert_eq!(detail.session.inbox.len(), 1);
        assert_eq!(detail.session.inbox[0].text, "review the diff");

        // Unknown agent → 404; empty message → 422.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/ghost/invoke",
                &serde_json::json!({"message": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents/reviewer/invoke",
                &serde_json::json!({"message": "   "}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // An agent naming a disconnected vendor is storable but not invocable.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/agents",
                &serde_json::json!({"name": "remote", "model": "mock", "vendor": "ghost-vendor"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let res = app
            .oneshot(post_json(
                "/api/agents/remote/invoke",
                &serde_json::json!({"message": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let _: AgentView = serde_json::from_value(body).unwrap_err(); // silence unused-import-style lint via type use
    }
```

(The final odd line is not needed — drop it; `AgentView` import is used above. Keep imports accurate while implementing.)

Note: `test_state` registers a `mock` vendor, so invoke with `vendor: "mock"` passes the connected check. If the invoke test's inbox assertion proves timing-sensitive (message consumed by a turn before the GET), assert `inbox.len() + history` instead — check actual behavior when running.

- [ ] **Step 4: Run the HTTP tests**

Run: `cargo test --workspace -- http::`
Expected: PASS (new + existing).

- [ ] **Step 5: Commit**

```bash
git add server/src/http/agents.rs server/src/http/mod.rs server/src/bin/horsie-server/main.rs
git commit -m "feat(server): agents CRUD + invoke endpoints"
```

---

### Task 6: CLI — `server_client`, `horsie agent list|get|invoke`, `horsie session list|status`

**Files:**
- Create: `cli/src/server_client.rs`
- Create: `cli/src/agent.rs`
- Modify: `cli/src/session.rs` (add `list` + `status`)
- Modify: `cli/src/lib.rs` (add `pub mod agent; pub mod server_client;`)
- Modify: `cli/src/main.rs` (Agent command group, Session List/Status, use shared `truncate`)

**Interfaces:**
- Consumes: `horsie_models::agents::*`, `horsie_models::session::{SessionSummary, SessionDetail}`, `horsie_models::session_api::{ApiError, ListSessionsResponse, GetSessionResponse}`.
- Produces:
  ```rust
  // server_client.rs
  pub struct ServerClient { ... }
  impl ServerClient {
      pub fn new(server: &str) -> Self;
      pub fn base(&self) -> &str;
      pub async fn list_agents(&self) -> Result<Vec<AgentView>, CliError>;
      pub async fn get_agent(&self, name: &str) -> Result<AgentView, CliError>;
      pub async fn invoke_agent(&self, name: &str, req: &AgentInvokeRequest) -> Result<AgentInvokeResponse, CliError>;
      pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CliError>;
      pub async fn get_session(&self, id: &str) -> Result<SessionDetail, CliError>;
  }
  // agent.rs
  pub async fn list(server: &str) -> Result<(), CliError>;
  pub async fn get(server: &str, name: &str) -> Result<(), CliError>;
  pub async fn invoke(server: &str, name: &str, message: String, session_name: Option<String>) -> Result<(), CliError>;
  pub fn truncate(s: &str, max: usize) -> String;  // moved from main.rs, reused by marketplace output
  // session.rs (additions)
  pub async fn list(server: &str) -> Result<(), CliError>;
  pub async fn status(server: &str, session_id: &str) -> Result<(), CliError>;
  ```

- [ ] **Step 1: Write `cli/src/server_client.rs`**

```rust
//! Minimal REST client for `horsie-server`, used by the agent and session
//! commands. Wire types come from `horsie_models` — no hand-rolled JSON.

use crate::error::CliError;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentView};
use horsie_models::session::{SessionDetail, SessionSummary};
use horsie_models::session_api::{ApiError, GetSessionResponse, ListSessionsResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct ServerClient {
    base: String,
    http: reqwest::Client,
}

impl ServerClient {
    pub fn new(server: &str) -> Self {
        Self {
            base: server.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// One JSON round-trip. Non-2xx → the server's `ApiError` message;
    /// transport failure → a "cannot reach server" error naming the base URL.
    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, CliError> {
        let url = format!("{}{path}", self.base);
        let mut req = self.http.request(method, &url);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| CliError::Server(format!("cannot reach server at {}: {e}", self.base)))?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| CliError::Server(format!("read response from {url}: {e}")))?;
        if !status.is_success() {
            let message = serde_json::from_slice::<ApiError>(&bytes)
                .map(|e| e.message)
                .unwrap_or_else(|_| format!("{status} {}", String::from_utf8_lossy(&bytes)));
            return Err(CliError::Server(message));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| CliError::Server(format!("bad response from {url}: {e}")))
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentView>, CliError> {
        self.send(reqwest::Method::GET, "/api/agents", None::<&str>).await
    }

    pub async fn get_agent(&self, name: &str) -> Result<AgentView, CliError> {
        self.send(reqwest::Method::GET, &format!("/api/agents/{name}"), None::<&str>)
            .await
    }

    pub async fn invoke_agent(
        &self,
        name: &str,
        req: &AgentInvokeRequest,
    ) -> Result<AgentInvokeResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/api/agents/{name}/invoke"),
            Some(req),
        )
        .await
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CliError> {
        let resp: ListSessionsResponse = self
            .send(reqwest::Method::GET, "/api/sessions", None::<&str>)
            .await?;
        Ok(resp.sessions)
    }

    pub async fn get_session(&self, id: &str) -> Result<SessionDetail, CliError> {
        let resp: GetSessionResponse = self
            .send(reqwest::Method::GET, &format!("/api/sessions/{id}"), None::<&str>)
            .await?;
        Ok(resp.session)
    }
}
```

- [ ] **Step 2: Write `cli/src/agent.rs` (tests first)**

```rust
//! `horsie agent …` commands: list/get agent presets and invoke one with a
//! message, printing the new session id and its web link.

use crate::error::CliError;
use crate::server_client::ServerClient;
use horsie_models::agents::{AgentInvokeRequest, AgentView};

/// Clip `s` to `max` display columns, marking elision with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let kept: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

pub async fn list(server: &str) -> Result<(), CliError> {
    let agents = ServerClient::new(server).list_agents().await?;
    print!("{}", render_agent_table(&agents));
    Ok(())
}

pub async fn get(server: &str, name: &str) -> Result<(), CliError> {
    let agent = ServerClient::new(server).get_agent(name).await?;
    print!("{}", render_agent_detail(&agent));
    Ok(())
}

/// Invoke an agent: the server creates the session and queues the message;
/// we print the session id and its web link as soon as it answers.
pub async fn invoke(
    server: &str,
    name: &str,
    message: String,
    session_name: Option<String>,
) -> Result<(), CliError> {
    let client = ServerClient::new(server);
    let res = client
        .invoke_agent(
            name,
            &AgentInvokeRequest {
                message,
                name: session_name,
            },
        )
        .await?;
    print!("{}", render_invoke(client.base(), &res.session.id));
    Ok(())
}

fn render_agent_table(agents: &[AgentView]) -> String {
    if agents.is_empty() {
        return "no agents\n".to_string();
    }
    let mut out = format!(
        "{:<20} {:<14} {:<12} {:>6} {:>7}  DESCRIPTION\n",
        "NAME", "MODEL", "VENDOR", "SKILLS", "MEMORY"
    );
    for a in agents {
        out.push_str(&format!(
            "{:<20} {:<14} {:<12} {:>6} {:>7}  {}\n",
            truncate(&a.name, 20),
            truncate(&a.model, 14),
            a.vendor.as_deref().unwrap_or("-"),
            a.plugins.len(),
            a.memory_spaces.len(),
            truncate(&a.description, 60),
        ));
    }
    out
}

fn render_agent_detail(a: &AgentView) -> String {
    let mut out = format!(
        "name        {}\ndescription {}\nmodel       {}\nvendor      {}\n",
        a.name,
        a.description,
        a.model,
        a.vendor.as_deref().unwrap_or("-"),
    );
    if let Some(e) = a.thinking_effort.as_deref() {
        out.push_str(&format!("thinking    {e}\n"));
    }
    for r in &a.repos {
        let git_ref = r.git_ref.as_deref().map(|g| format!(" @ {g}")).unwrap_or_default();
        out.push_str(&format!("repo        {}{git_ref}\n", r.url));
    }
    if !a.plugins.is_empty() {
        out.push_str(&format!("skills      {}\n", a.plugins.join(", ")));
    }
    if !a.mcp_servers.is_empty() {
        out.push_str(&format!("mcp         {}\n", a.mcp_servers.join(", ")));
    }
    if !a.memory_spaces.is_empty() {
        out.push_str(&format!("memory      {}\n", a.memory_spaces.join(", ")));
    }
    out
}

/// Two lines: the bare id (script-friendly) and the clickable web link.
fn render_invoke(base: &str, session_id: &str) -> String {
    format!("session {session_id}\n{base}/sessions/{session_id}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentView {
        AgentView {
            name: name.into(),
            description: "reviews PRs".into(),
            vendor: Some("local".into()),
            model: "sonnet".into(),
            repos: vec![],
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn empty_table_says_no_agents() {
        assert_eq!(render_agent_table(&[]), "no agents\n");
    }

    #[test]
    fn table_has_header_and_one_row_per_agent() {
        let out = render_agent_table(&[agent("reviewer"), agent("fixer")]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("NAME"));
        assert!(lines[1].contains("reviewer"));
        assert!(lines[1].contains("sonnet"));
        assert!(lines[2].contains("fixer"));
    }

    #[test]
    fn detail_lists_skills_and_memory() {
        let out = render_agent_detail(&agent("reviewer"));
        assert!(out.contains("name        reviewer"));
        assert!(out.contains("skills      superpowers"));
        assert!(out.contains("memory      default"));
        assert!(!out.contains("mcp"), "empty lists are omitted: {out}");
    }

    #[test]
    fn invoke_output_is_id_then_link() {
        let out = render_invoke("http://127.0.0.1:3789", "abc-123");
        assert_eq!(
            out,
            "session abc-123\nhttp://127.0.0.1:3789/sessions/abc-123\n"
        );
    }

    #[test]
    fn truncate_marks_elision_and_flattens_newlines() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a much longer description", 10), "a much lo…");
        assert_eq!(truncate("line\nbreak", 20), "line break");
    }
}
```

- [ ] **Step 3: Extend `cli/src/session.rs` with `list` and `status`**

Update the module doc to `//! Commands against `horsie-server`: tail a session's events to JSONL, list sessions, show one session's status.` Add:

```rust
use crate::server_client::ServerClient;
use horsie_models::session::{SessionDetail, SessionSummary};

/// `horsie session list` — every session the server knows about.
pub async fn list(server: &str) -> Result<(), CliError> {
    let sessions = ServerClient::new(server).list_sessions().await?;
    print!("{}", render_session_table(&sessions, now_ms()));
    Ok(())
}

/// `horsie session status <id>` — a point-in-time snapshot (live progress is
/// `session tail`'s job).
pub async fn status(server: &str, session_id: &str) -> Result<(), CliError> {
    let detail = ServerClient::new(server).get_session(session_id).await?;
    print!("{}", render_session_detail(&detail, now_ms()));
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// "just now", "5m ago", "3h ago", "2d ago".
fn relative(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn status_label(s: &Option<horsie_models::session::SessionStatusKind>) -> String {
    s.as_ref()
        .map(|k| format!("{k:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn render_session_table(sessions: &[SessionSummary], now: u64) -> String {
    if sessions.is_empty() {
        return "no sessions\n".to_string();
    }
    let mut out = format!(
        "{:<38} {:<24} {:<16} {:<10} LAST ERROR\n",
        "ID", "NAME", "STATUS", "CREATED"
    );
    for s in sessions {
        out.push_str(&format!(
            "{:<38} {:<24} {:<16} {:<10} {}\n",
            s.id,
            crate::agent::truncate(s.name.as_deref().unwrap_or("-"), 24),
            status_label(&s.status),
            relative(now, s.created_at),
            s.last_error.as_deref().unwrap_or(""),
        ));
    }
    out
}

fn render_session_detail(d: &SessionDetail, now: u64) -> String {
    let mut out = format!(
        "session     {}\nname        {}\nstatus      {}\ncreated     {}\nmodel       {}\nvendor      {}\n",
        d.id,
        d.name.as_deref().unwrap_or("-"),
        status_label(&d.status),
        relative(now, d.created_at),
        d.model,
        d.vendor,
    );
    if let Some(e) = d.thinking_effort.as_deref() {
        out.push_str(&format!("thinking    {e}\n"));
    }
    for r in &d.repos {
        out.push_str(&format!("repo        {r}\n"));
    }
    if !d.plugins.is_empty() {
        out.push_str(&format!("skills      {}\n", d.plugins.join(", ")));
    }
    if !d.mcp_servers.is_empty() {
        out.push_str(&format!("mcp         {}\n", d.mcp_servers.join(", ")));
    }
    if !d.memory_spaces.is_empty() {
        out.push_str(&format!("memory      {}\n", d.memory_spaces.join(", ")));
    }
    if let Some(err) = d.last_error.as_deref() {
        out.push_str(&format!("error       {err}\n"));
    }
    if let Some(q) = d.pending_question.as_deref() {
        out.push_str(&format!("awaiting    {q}\n"));
    }
    if !d.inbox.is_empty() {
        out.push_str(&format!("inbox       {} queued\n", d.inbox.len()));
        for m in &d.inbox {
            out.push_str(&format!("  · {}\n", crate::agent::truncate(&m.text, 70)));
        }
    }
    out
}
```

Add tests to the existing `tests` mod in `session.rs`:

```rust
    fn summary(id: &str, name: Option<&str>) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            name: name.map(Into::into),
            status: Some(horsie_models::session::SessionStatusKind::Working),
            created_at: 1_000,
            last_error: None,
        }
    }

    #[test]
    fn session_table_lists_status_and_relative_time() {
        let out = render_session_table(&[summary("s-1", Some("review"))], 1_000 + 5 * 60_000);
        assert!(out.contains("s-1"));
        assert!(out.contains("review"));
        assert!(out.contains("Working"));
        assert!(out.contains("5m ago"));
    }

    #[test]
    fn relative_buckets() {
        assert_eq!(relative(10_000, 0), "just now");
        assert_eq!(relative(5 * 60_000, 0), "5m ago");
        assert_eq!(relative(3 * 3_600_000, 0), "3h ago");
        assert_eq!(relative(2 * 86_400_000, 0), "2d ago");
    }

    #[test]
    fn detail_shows_awaiting_question_and_inbox() {
        let d = SessionDetail {
            id: "s-1".into(),
            name: None,
            status: Some(horsie_models::session::SessionStatusKind::AwaitingInput),
            created_at: 0,
            last_error: None,
            pending_question: Some("which file?".into()),
            model: "sonnet".into(),
            vendor: "local".into(),
            repos: vec![],
            plugins: vec![],
            mcp_servers: vec![],
            memory_spaces: vec![],
            use_plugins: false,
            thinking_effort: None,
            inbox: vec![horsie_models::session::QueuedMessage {
                id: "m1".into(),
                text: "follow up".into(),
                at_ms: 0,
            }],
        };
        let out = render_session_detail(&d, 0);
        assert!(out.contains("awaiting    which file?"));
        assert!(out.contains("inbox       1 queued"));
        assert!(out.contains("· follow up"));
    }
```

(Verify `SessionStatusKind` variant names — `Working`, `AwaitingInput` — against `models/fluorite/session.fl` while implementing.)

- [ ] **Step 4: Wire `main.rs` + `lib.rs`**

`cli/src/lib.rs`: add `pub mod agent;` and `pub mod server_client;` (alphabetical).

`cli/src/main.rs`:
- Delete the local `truncate` fn; marketplace output uses `horsie::agent::truncate` (add `use horsie::agent::truncate;`… note main.rs already does `use horsie::session::{self, EventsMode};` — add `use horsie::agent;` and call `agent::truncate(...)` in `MarketplaceAction::Show`).
- Add the command group after `Session`:

```rust
    /// List and invoke agent presets on a session server (`horsie-server`).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
```

```rust
#[derive(Subcommand)]
enum AgentAction {
    /// List agent presets.
    List {
        /// Session server base URL.
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
    },
    /// Show one agent preset.
    Get {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
    },
    /// Invoke an agent with a message: creates a session and prints its id
    /// and web link immediately.
    Invoke {
        name: String,
        /// First user message (required).
        #[arg(short = 'm', long)]
        message: String,
        /// Optional session title.
        #[arg(long)]
        session_name: Option<String>,
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
    },
}
```

- Add to `SessionAction`:

```rust
    /// List sessions on the server.
    List {
        /// Session server base URL.
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
    },
    /// Show a session's current status (point-in-time snapshot).
    Status {
        /// Session UUID on the server.
        session_id: String,
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
    },
```

- Add dispatch arms:

```rust
        Command::Agent { action } => match action {
            AgentAction::List { server } => {
                agent::list(&server).await?;
                Ok(0)
            }
            AgentAction::Get { name, server } => {
                agent::get(&server, &name).await?;
                Ok(0)
            }
            AgentAction::Invoke {
                name,
                message,
                session_name,
                server,
            } => {
                agent::invoke(&server, &name, message, session_name).await?;
                Ok(0)
            }
        },
```

and in `Command::Session`:

```rust
            SessionAction::List { server } => {
                session::list(&server).await?;
                Ok(0)
            }
            SessionAction::Status { session_id, server } => {
                session::status(&server, &session_id).await?;
                Ok(0)
            }
```

- [ ] **Step 5: Run CLI tests**

Run: `cargo test --workspace -- horsie::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add cli/src/server_client.rs cli/src/agent.rs cli/src/session.rs cli/src/lib.rs cli/src/main.rs
git commit -m "feat(cli): agent list/get/invoke + session list/status commands"
```

---

### Task 7: Web data layer — api client + `useAgents` hook

**Files:**
- Modify: `clients/web/src/api/client.ts` (add `api.agents`, import types)
- Create: `clients/web/src/hooks/useAgents.ts`

**Interfaces:**
- Consumes: generated `AgentView`/`AgentInput` from `../api/types` (Task 1).
- Produces:
  ```ts
  api.agents.list(): Promise<AgentView[]>
  api.agents.get(name): Promise<AgentView>
  api.agents.create(body: AgentInput): Promise<AgentView>
  api.agents.update(name, body: AgentInput): Promise<AgentView>
  api.agents.remove(name): Promise<void>
  // hooks
  export const agentKeys = { all: ["agents"] as const, one: (name: string) => ["agents", name] as const };
  useAgents(): UseQueryResult<AgentView[]>
  useAgent(name: string | undefined): UseQueryResult<AgentView>
  useCreateAgent(): UseMutationResult<AgentView, ..., AgentInput>
  useUpdateAgent(): UseMutationResult<AgentView, ..., { name: string; body: AgentInput }>
  useDeleteAgent(): UseMutationResult<void, ..., string>
  ```

- [ ] **Step 1: Add `api.agents` to `client.ts`**

Add `AgentInput, AgentView` to the type imports, and after the `sessions` block:

```ts
  agents: {
    /** All agent presets. */
    list: (): Promise<AgentView[]> => request("/agents"),

    get: (name: string): Promise<AgentView> =>
      request(`/agents/${encodeURIComponent(name)}`),

    create: (body: AgentInput): Promise<AgentView> =>
      request("/agents", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. */
    update: (name: string, body: AgentInput): Promise<AgentView> =>
      request(`/agents/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/agents/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },
```

- [ ] **Step 2: Write `clients/web/src/hooks/useAgents.ts`**

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { AgentInput } from "../api/types";

export const agentKeys = {
  all: ["agents"] as const,
  one: (name: string) => ["agents", name] as const,
};

/** All agent presets. */
export function useAgents() {
  return useQuery({ queryKey: agentKeys.all, queryFn: () => api.agents.list() });
}

export function useAgent(name: string | undefined) {
  return useQuery({
    queryKey: name ? agentKeys.one(name) : ["agents", "none"],
    queryFn: () => api.agents.get(name as string),
    enabled: !!name,
  });
}

export function useCreateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: AgentInput) => api.agents.create(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: agentKeys.all }),
  });
}

export function useUpdateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: AgentInput }) =>
      api.agents.update(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: agentKeys.all });
      qc.invalidateQueries({ queryKey: agentKeys.one(name) });
    },
  });
}

export function useDeleteAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.agents.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: agentKeys.all }),
  });
}
```

- [ ] **Step 3: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/api/client.ts clients/web/src/hooks/useAgents.ts
git commit -m "feat(web): agents api client + useAgents hooks"
```

---

### Task 8: `ConfigDraft` narrowing + `useAgentDraft`

**Files:**
- Modify: `clients/web/src/hooks/useSessionDraft.ts` (extract `ConfigDraft` interface; `SessionDraft extends ConfigDraft`)
- Modify: `clients/web/src/components/SessionConfigBar.tsx` (prop type `ConfigDraft`)
- Create: `clients/web/src/hooks/useAgentDraft.ts`
- Create: `clients/web/src/hooks/useAgentDraft.test.tsx`

**Interfaces:**
- Produces:
  ```ts
  // useSessionDraft.ts
  export interface ConfigDraft {
    vendor: string; setVendor: (v: string) => void;
    model: string; setModel: (m: string) => void;
    repos: Map<string, string>; setRepos: (m: Map<string, string>) => void;
    skills: Set<string>; setSkills: (s: Set<string>) => void;
    mcp: Set<string>; setMcp: (s: Set<string>) => void;
    memorySpaces: Set<string>; setMemorySpaces: (s: Set<string>) => void;
    thinkingEffort: string; setThinkingEffort: (e: string) => void;
    thinkingEfforts: string[];
    modelDefaultThinkingEffort: string;
    provisions: boolean;
    githubConnected: boolean;
  }
  export interface SessionDraft extends ConfigDraft {
    canSend: boolean;
    blockedReason: string | null;
    buildRequest: () => CreateSessionRequest;
  }
  // useAgentDraft.ts
  export interface AgentDraft extends ConfigDraft {
    buildAgentInput: (name: string, description: string) => AgentInput;
  }
  export function useAgentDraft(initial?: AgentView): AgentDraft;
  ```

- [ ] **Step 1: Refactor `useSessionDraft.ts`**

Split the existing `SessionDraft` interface into `ConfigDraft` (everything except `canSend`, `blockedReason`, `buildRequest`) and `SessionDraft extends ConfigDraft` with those three. No implementation change.

- [ ] **Step 2: Re-type `SessionConfigBar`**

In `SessionConfigBar.tsx`: import `ConfigDraft` instead of `SessionDraft`; change `DraftControls({ draft }: { draft: ConfigDraft })` and the `Props` union to `{ mode: "draft"; draft: ConfigDraft }`.

Run: `cd clients/web && bun run typecheck && bun run test:unit`
Expected: PASS (existing `SessionConfigBar.test.tsx` + `useSessionDraft.test.tsx` guard the refactor).

- [ ] **Step 3: Write `useAgentDraft.ts` + failing test**

```ts
import { useMemo, useState } from "react";
import type { AgentInput, AgentView, RepoConfig } from "../api/types";
import { useGithubStatus } from "./useGithub";
import { useSettings } from "./useSettings";
import type { ConfigDraft } from "./useSessionDraft";

export interface AgentDraft extends ConfigDraft {
  /** Assemble the save payload. `name`/`description` come from the form's
   * text inputs, not the picker state. */
  buildAgentInput: (name: string, description: string) => AgentInput;
}

/** `https://github.com/org/repo` → `org/repo`; anything else is kept whole. */
function fullName(url: string): string {
  return url.replace(/^https:\/\/github\.com\//, "").replace(/\.git$/, "");
}

/** Agent-preserving draft state for the agent edit form. Unlike
 * `useSessionDraft` nothing persists to localStorage and there is no
 * first-visit seeding — the preset being edited (or empty defaults) is the
 * source of truth. */
export function useAgentDraft(initial?: AgentView): AgentDraft {
  const { data: settings } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const [vendor, setVendor] = useState(initial?.vendor ?? "");
  const [model, setModel] = useState(initial?.model ?? "");
  const [repos, setRepos] = useState<Map<string, string>>(
    () =>
      new Map(
        (initial?.repos ?? []).map((r) => [fullName(r.url), r.gitRef ?? ""]),
      ),
    );
  const [skills, setSkills] = useState<Set<string>>(
    () => new Set(initial?.plugins ?? []),
  );
  const [mcp, setMcp] = useState<Set<string>>(
    () => new Set(initial?.mcpServers ?? []),
  );
  const [memorySpaces, setMemorySpaces] = useState<Set<string>>(
    () => new Set(initial?.memorySpaces ?? []),
  );
  const [thinkingEffort, setThinkingEffort] = useState(
    initial?.thinkingEffort ?? "",
  );

  const activeVendors = settings?.vendors ?? [];
  const selectedVendor = activeVendors.find(
    (v) => v.name === (vendor || settings?.defaultVendor),
  );
  const provisions = !!selectedVendor?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  const selectedModel = (settings?.models ?? []).find((m) => m.alias === model);
  const thinkingEfforts = selectedModel?.thinkingEfforts ?? [];
  const effectiveThinkingEffort = thinkingEfforts.includes(thinkingEffort)
    ? thinkingEffort
    : "";

  const buildAgentInput = useMemo(
    () =>
      (name: string, description: string): AgentInput => {
        const repoList: RepoConfig[] = provisions
          ? [...repos.entries()].map(([fn, ref]) => ({
              url: `https://github.com/${fn}`,
              gitRef: ref.trim() || undefined,
            }))
          : [];
        return {
          name: name.trim(),
          description: description.trim() || undefined,
          vendor: vendor.trim() || undefined,
          model: model.trim(),
          repos: repoList.length ? repoList : undefined,
          plugins: provisions && skills.size ? [...skills] : undefined,
          mcpServers: provisions && mcp.size ? [...mcp] : undefined,
          memorySpaces: memorySpaces.size ? [...memorySpaces] : undefined,
          thinkingEffort: effectiveThinkingEffort || undefined,
        };
      },
    [provisions, repos, vendor, model, skills, mcp, memorySpaces, effectiveThinkingEffort],
  );

  return {
    vendor, setVendor,
    model, setModel,
    repos: new Map(repos), setRepos: (m) => setRepos(new Map(m)),
    skills: new Set(skills), setSkills: (s) => setSkills(new Set(s)),
    mcp: new Set(mcp), setMcp: (s) => setMcp(new Set(s)),
    memorySpaces: new Set(memorySpaces),
    setMemorySpaces: (s) => setMemorySpaces(new Set(s)),
    thinkingEffort: effectiveThinkingEffort,
    setThinkingEffort,
    thinkingEfforts,
    modelDefaultThinkingEffort: selectedModel?.thinkingEffort ?? "",
    provisions,
    githubConnected,
    buildAgentInput,
  };
}
```

Test `useAgentDraft.test.tsx` — mirror `useSessionDraft.test.tsx`'s renderHook + QueryClientProvider wrapper pattern (read that file first):

```tsx
// Cases to cover (mock useSettings/useGithubStatus via vi.mock as
// useSessionDraft.test.tsx does, or seed a QueryClient with settingsKey data):
// 1. Empty initial → blank draft; buildAgentInput produces minimal input.
// 2. Initial AgentView → all fields populated; repo url mapped back to fullName.
// 3. buildAgentInput strips github prefix into RepoConfig urls, keeps gitRef,
//    omits repos/plugins/mcp when the vendor doesn't provision, omits empty lists.
// 4. A thinkingEffort the model doesn't offer falls back to "" (default).
```

- [ ] **Step 4: Run unit tests**

Run: `cd clients/web && bun run test:unit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/hooks/useSessionDraft.ts clients/web/src/components/SessionConfigBar.tsx clients/web/src/hooks/useAgentDraft.ts clients/web/src/hooks/useAgentDraft.test.tsx
git commit -m "feat(web): ConfigDraft narrowing + useAgentDraft for the agent form"
```

---

### Task 9: Agents pages + sidebar + routes

**Files:**
- Create: `clients/web/src/pages/agents/AgentsPage.tsx`
- Create: `clients/web/src/pages/agents/AgentEditPage.tsx`
- Create: `clients/web/src/pages/agents/AgentsPage.test.tsx`
- Modify: `clients/web/src/components/Sidebar.tsx` (Agents section above session list)
- Modify: `clients/web/src/App.tsx` (routes)

**Interfaces:**
- Consumes: `useAgents`, `useAgent`, `useCreateAgent`, `useUpdateAgent`, `useDeleteAgent` (Task 7); `useAgentDraft` (Task 8); `SessionConfigBar mode="draft"`.

- [ ] **Step 1: `AgentsPage.tsx`**

```tsx
import { Bot, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
import { EmptyState } from "../../components/EmptyState";
import { useAgents, useDeleteAgent } from "../../hooks/useAgents";

export function AgentsPage() {
  const { data: agents, isLoading, isError } = useAgents();
  const del = useDeleteAgent();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="agents-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="text-[15px] font-semibold text-text">Agents</h1>
        <button
          className="btn-primary ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => navigate("/agents/new")}
          data-testid="new-agent-button"
        >
          <Plus size={15} />
          New agent
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && <p className="text-sm text-error">Can’t reach the server.</p>}
        {agents && agents.length === 0 && (
          <EmptyState icon={<Bot size={24} />} title="No agents yet">
            An agent is a saved session setup — runtime, model, repos, skills,
            memory — that you can invoke from the CLI with{" "}
            <code>horsie agent invoke &lt;name&gt; -m "…"</code>.
          </EmptyState>
        )}
        <div className="space-y-2">
          {(agents ?? []).map((a) => (
            <div
              key={a.name}
              className="flex items-center gap-3 rounded-[var(--radius)] border px-4 py-3"
              data-testid="agent-row"
              data-agent-name={a.name}
            >
              <Link
                to={`/agents/${encodeURIComponent(a.name)}/edit`}
                className="min-w-0 flex-1"
              >
                <div className="flex items-baseline gap-2">
                  <span className="font-mono text-sm font-medium text-text">
                    {a.name}
                  </span>
                  <span className="text-xs text-faint">
                    {a.model} · {a.vendor ?? "default vendor"}
                  </span>
                </div>
                {a.description && (
                  <div className="truncate text-sm text-muted">{a.description}</div>
                )}
                <div className="mt-1 flex gap-2 text-[11px] text-faint">
                  {a.plugins.length > 0 && <span>{a.plugins.length} skills</span>}
                  {a.memorySpaces.length > 0 && <span>{a.memorySpaces.length} memory</span>}
                  {a.mcpServers.length > 0 && <span>{a.mcpServers.length} MCP</span>}
                  {a.repos.length > 0 && <span>{a.repos.length} repos</span>}
                </div>
              </Link>
              <button
                className="rounded-[var(--radius-sm)] p-1.5 text-faint hover:bg-surface-2 hover:text-error"
                title={`Delete ${a.name}`}
                data-testid={`delete-agent-${a.name}`}
                onClick={() => {
                  if (window.confirm(`Delete agent '${a.name}'?`)) del.mutate(a.name);
                }}
              >
                <Trash2 size={15} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: `AgentEditPage.tsx`**

```tsx
import { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import { SessionConfigBar } from "../../components/SessionConfigBar";
import { useAgent, useCreateAgent, useUpdateAgent } from "../../hooks/useAgents";
import { useAgentDraft } from "../../hooks/useAgentDraft";

export function AgentEditPage() {
  const { name } = useParams<{ name: string }>();
  const editing = !!name;
  const { data: existing, isLoading } = useAgent(name);
  const create = useCreateAgent();
  const update = useUpdateAgent();
  const navigate = useNavigate();
  const [agentName, setAgentName] = useState("");
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (existing) {
      setAgentName(existing.name);
      setDescription(existing.description);
    }
  }, [existing]);

  const draft = useAgentDraft(existing);
  const busy = create.isPending || update.isPending;
  const canSave = !busy && agentName.trim() !== "" && draft.model.trim() !== "";

  const handleSave = async () => {
    setError(null);
    const body = draft.buildAgentInput(agentName, description);
    try {
      if (editing) await update.mutateAsync({ name: agentName.trim(), body });
      else await create.mutateAsync(body);
      navigate("/agents");
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to save agent.");
    }
  };

  if (editing && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }

  return (
    <div className="flex h-full flex-col" data-testid="agent-edit-page">
      <div className="border-b px-6 py-4">
        <h1 className="text-[15px] font-semibold text-text">
          {editing ? `Edit ${name}` : "New agent"}
        </h1>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="mx-auto w-full max-w-3xl space-y-4">
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">Name</span>
            <input
              className="input w-full font-mono"
              placeholder="reviewer"
              value={agentName}
              disabled={editing}
              onChange={(e) => setAgentName(e.target.value)}
              data-testid="agent-name-input"
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">
              Description
            </span>
            <input
              className="input w-full"
              placeholder="What this agent is for"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              data-testid="agent-description-input"
            />
          </label>
          {error && (
            <div
              className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
              data-testid="agent-error"
            >
              {error}
            </div>
          )}
        </div>
      </div>
      <SessionConfigBar mode="draft" draft={draft} />
      <div className="mx-auto flex w-full max-w-3xl gap-2 px-4 pb-4">
        <button
          className="btn-primary"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-agent-button"
        >
          {busy ? "Saving…" : "Save agent"}
        </button>
        <button className="btn-secondary" onClick={() => navigate("/agents")}>
          Cancel
        </button>
      </div>
    </div>
  );
}
```

(Verify `btn-secondary` exists in the codebase styles; otherwise reuse the muted-button classes from an existing settings page.)

- [ ] **Step 3: Sidebar section + routes**

In `Sidebar.tsx`, between the search box and the sessions `<nav>`: an Agents NavLink styled like the Settings/Admin links, plus a faint "Sessions" label above the list:

```tsx
      <div className="px-2 pb-1">
        <NavLink
          to="/agents"
          data-testid="agents-link"
          className={({ isActive }) =>
            cn(
              "flex items-center gap-2.5 rounded-[var(--radius)] px-2.5 py-2 text-sm transition-colors",
              isActive
                ? "bg-surface-3 text-text"
                : "text-muted hover:bg-surface-2 hover:text-text",
            )
          }
        >
          <Bot size={15} />
          <span className="font-medium">Agents</span>
        </NavLink>
      </div>
      <div className="px-4 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wide text-faint">
        Sessions
      </div>
```

(Add `Bot` to the lucide imports.)

In `App.tsx`, under the `SessionsLayout` route (after `sessions/:id`):

```tsx
            <Route path="agents" element={<AgentsPage />} />
            <Route path="agents/new" element={<AgentEditPage />} />
            <Route path="agents/:name/edit" element={<AgentEditPage />} />
```

(with the imports).

- [ ] **Step 4: `AgentsPage.test.tsx`**

Mirror existing component tests (`SessionConfigBar.test.tsx`): render inside `QueryClientProvider` + `MemoryRouter`, `vi.mock("../../api/client", ...)` so `api.agents.list` resolves two fixtures and `api.agents.remove` resolves void. Assert: rows render with name/model/description; clicking delete (stub `window.confirm` → true) calls `api.agents.remove` with the name.

Run: `cd clients/web && bun run typecheck && bun run test:unit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/pages/agents clients/web/src/components/Sidebar.tsx clients/web/src/App.tsx
git commit -m "feat(web): agents management page + sidebar section"
```

---

### Task 10: Web e2e — agents CRUD happy path

**Files:**
- Create: `clients/web/e2e/j-agents.spec.ts`

**Interfaces:**
- Consumes: `fixtures.ts` (`test`, `appBase`), real server from global-setup.

- [ ] **Step 1: Write the spec**

```ts
import { expect, test } from "./fixtures";

// Agents CRUD: create via the API (a model must exist for save-time
// validation — discover the harness's model alias from /api/config), then
// list, edit, and delete through the UI.
test("agents page lists, edits, and deletes an agent", async ({
  page,
  appBase,
}) => {
  const cfg = (await (
    await page.request.get(`${appBase}/api/config`)
  ).json()) as { models: { alias: string }[] };
  const alias = cfg.models[0]?.alias;
  test.skip(!alias, "e2e harness has no configured model");

  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name: "e2e-agent", model: alias, description: "from e2e" },
  });
  expect(res.status()).toBe(201);

  await page.goto(`${appBase}/agents`);
  const row = page.getByTestId("agent-row");
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("e2e-agent");
  await expect(row).toContainText("from e2e");
  await expect(row).toContainText(alias);

  // Edit the description through the form.
  await row.getByRole("link").click();
  await expect(page.getByTestId("agent-edit-page")).toBeVisible();
  await expect(page.getByTestId("agent-name-input")).toBeDisabled();
  await page.getByTestId("agent-description-input").fill("edited");
  await page.getByTestId("save-agent-button").click();
  await expect(page.getByTestId("agent-row")).toContainText("edited");

  // Delete with the confirm accepted.
  page.on("dialog", (d) => void d.accept());
  await page.getByTestId("delete-agent-e2e-agent").click();
  await expect(page.getByTestId("agent-row")).toHaveCount(0);
});
```

(Adjust selectors to what Task 9 actually renders; check `e2e/global-setup.ts` for how the harness configures models first.)

- [ ] **Step 2: Run the e2e spec**

Run: `cd clients/web && bun run test:e2e -- j-agents.spec.ts`
Expected: PASS. If the harness boots slowly or the spec is flaky in this environment, run it once locally and record the result honestly in the PR body.

- [ ] **Step 3: Commit**

```bash
git add clients/web/e2e/j-agents.spec.ts
git commit -m "test(web): e2e for agents CRUD happy path"
```

---

### Task 11: Full gates + PR

- [ ] **Step 1: Rust gates**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check   # stable toolchain (rustup default), never nightly
cargo test --workspace
```
Expected: all PASS.

- [ ] **Step 2: Web gates**

```bash
cd clients/web && bun run typecheck && bun run test:unit && bun run build
```
Expected: PASS.

- [ ] **Step 3: Push + open PR**

```bash
git push -u origin feat/agent-presets
gh pr create --title "feat: agent presets — server CRUD + invoke, CLI, web UI" --body "..."
```
Body: why/what + bullets (invoke semantics, save- vs invoke-time validation split, CLI output shape, ConfigDraft refactor). Watch CI (`gh pr checks --watch`) and fix any failures before calling it done.
