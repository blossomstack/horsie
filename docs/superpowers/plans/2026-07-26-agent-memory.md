# Agent Memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give horsie session agents durable, server-persisted memories they manage through a five-tool set, surfaced as a compact index in the system prompt and loaded on demand.

**Architecture:** A new `server/src/memory/` module owns a SQLite-backed store (`memory_spaces` + `memories` tables), a service layer, a prompt-index renderer, and a `MemoryToolbox` that executes entirely in the server process — no runtime, executor, or wire-protocol changes. Sessions select memory spaces at creation via `AgentSettings.memory_spaces`; `SessionContextProvider::provide()` wraps the agent's toolbox with `MemoryToolbox` and appends the rendered index to the system prompt.

**Tech Stack:** Rust (axum, sqlx 0.8 runtime queries, async-trait, serde_json), fluorite schema codegen, React 19 + TanStack Query 5 + Tailwind 4 in `clients/web`.

**Spec:** `docs/superpowers/specs/2026-07-26-agent-memory-design.md`

**Worktree:** `/Users/xiaoguang/works/repos/bloomstack/october/horsie-agent-memory`, branch `feat/agent-memory` off `origin/main` @ `47614b4`.

## Global Constraints

- **No panics in production code.** The workspace denies `clippy::unwrap_used` and `clippy::expect_used`. Test modules opt out with the `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` attribute block that sits atop every existing `mod tests`.
- **Store errors are `Result<_, String>`.** Every store and service method in this codebase returns `Result<T, String>`, mapping sqlx errors with `.map_err(|e| e.to_string())`. Follow it; do not introduce a new error enum.
- **sqlx runtime queries only.** Use `sqlx::query(...)` with `.bind(...)`, never the compile-time-checked `query!` macros — there is no `DATABASE_URL` or offline data in this repo.
- **Timestamps are `TEXT` holding unix epoch seconds**, generated in the service layer and passed to the store as `String`. Matches `0003_mcp.sql` and `server/src/plugins/store.rs`.
- **No SQL foreign keys.** `PRAGMA foreign_keys` is never enabled, so `REFERENCES` clauses are silently ignored. Enforce the space↔memory relationship in explicit transactions.
- **Journaled types need `#[serde(default)]`.** Any new field on `SessionSpec` or its nested `AgentSettings` must have it, or recovering an existing session fails to deserialize.
- **Tool specs must be static.** `CompositeToolbox::execute` calls `specs()` on every composed toolbox on every tool call. No database access in `specs()`.
- **Verify formatting through CI, not locally.** CI runs nightly rustfmt with import-wrapping settings that local stable `cargo fmt` silently accepts and local nightly reports false repo-wide diffs against.
- **Full gate before the PR:** `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, ts-drift for `clients/ts`, and `cd clients/web && bun run build`.
- **No AI attribution** in commits or the PR body.

## File Structure

**Created:**

| Path | Responsibility |
| --- | --- |
| `server/migrations/0008_memory.sql` | `memory_spaces` + `memories` tables, seeds the `default` space |
| `server/src/memory/mod.rs` | Module docs, re-exports, slug validation, `MAX_DESCRIPTION_CHARS` |
| `server/src/memory/store.rs` | `MemoryStore` — all SQL, transactional space delete/rename |
| `server/src/memory/service.rs` | `MemoryService` — validation, timestamps, row→view mapping |
| `server/src/memory/prompt.rs` | `render_index` — pure function producing the prompt block |
| `server/src/memory/toolbox.rs` | `MemoryToolbox` — the five agent tools, wraps an inner toolbox |
| `server/src/http/memory.rs` | Nine thin HTTP handlers |
| `models/fluorite/memory.fl` | Wire types |
| `clients/web/src/hooks/useMemory.ts` | TanStack Query hooks |
| `clients/web/src/pages/MemoryPage.tsx` | Management UI |

**Modified:** `models/src/lib.rs`, `server/src/lib.rs`, `server/src/http/mod.rs`, `server/src/http/handlers.rs`, `server/src/sessions/spec.rs`, `server/src/sessions/session_actor.rs`, `server/src/sessions/supervisor.rs`, `server/src/sessions/system_prompt.md`, `server/src/bin/horsie-server/main.rs`, `models/fluorite/session.fl`, `clients/web/package.json`, `clients/web/src/api/client.ts`, `clients/web/src/api/types.ts`, `clients/web/src/App.tsx`, `clients/web/src/components/Sidebar.tsx`, `clients/web/src/components/SessionConfigBar.tsx`, `clients/web/src/hooks/useSessionDraft.ts`.

**Task dependency order:** 1 → 2 → 3 (HTTP) and 4 (toolbox) in either order → 5 (session wiring, needs 4) → 6 (web management, needs 3) → 7 (web session picker, needs 5+6) → 8 (gate + PR).

---

### Task 1: Migration and `MemoryStore`

**Files:**
- Create: `server/migrations/0008_memory.sql`
- Create: `server/src/memory/mod.rs`
- Create: `server/src/memory/store.rs`
- Modify: `server/src/lib.rs` (add `pub mod memory;`)
- Test: in-file `mod tests` in `server/src/memory/store.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `MemoryStore::new(SqlitePool)`; row types `MemorySpaceRow { name: String, description: String, created_at: String, updated_at: String }` and `MemoryRow { id: i64, space: String, name: String, description: String, content: String, created_at: String, updated_at: String }`; methods `list_spaces() -> Result<Vec<MemorySpaceRow>, String>`, `get_space(&str) -> Result<Option<MemorySpaceRow>, String>`, `create_space(&MemorySpaceRow) -> Result<(), String>`, `update_space_description(&str, &str, &str) -> Result<bool, String>`, `rename_space(&str, &str, &str) -> Result<(), String>`, `delete_space(&str) -> Result<bool, String>`, `list_memories(Option<&str>) -> Result<Vec<MemoryRow>, String>`, `memories_in(&[String]) -> Result<Vec<MemoryRow>, String>`, `get_memory(i64) -> Result<Option<MemoryRow>, String>`, `get_memory_by_ref(&str, &str) -> Result<Option<MemoryRow>, String>`, `create_memory(&MemoryRow) -> Result<i64, String>`, `update_memory(i64, Option<&str>, Option<&str>, &str) -> Result<bool, String>`, `delete_memory(i64) -> Result<bool, String>`. Also `memory::validate_slug(&str) -> Result<(), String>` and `memory::MAX_DESCRIPTION_CHARS: usize`.

- [ ] **Step 1: Write the migration**

Create `server/migrations/0008_memory.sql`:

```sql
-- Agent-managed long-term memories, grouped into named spaces. Sessions select
-- spaces at creation; the agent sees an index of the memories in the selected
-- spaces and loads bodies on demand with the memory_load tool.
--
-- `memories.space` is deliberately NOT declared as a SQL foreign key: no table
-- in this schema uses REFERENCES and `PRAGMA foreign_keys` is never enabled in
-- `open_pool`, so a declared ON DELETE CASCADE would be silently ignored --
-- worse than no constraint at all. MemoryStore enforces the relationship in
-- explicit transactions instead (delete_space, rename_space, create_memory).

CREATE TABLE memory_spaces (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);

INSERT INTO memory_spaces (name, description, created_at, updated_at)
    VALUES ('default', 'Default memory space', strftime('%s','now'), strftime('%s','now'));

CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    space       TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (space, name)
);

CREATE INDEX idx_memories_space ON memories(space);
```

- [ ] **Step 2: Write `server/src/memory/mod.rs`**

```rust
//! Agent-managed long-term memories, grouped into named spaces. A session
//! selects spaces at creation; the agent sees a one-line-per-memory index in
//! its system prompt and loads full bodies on demand. Everything here executes
//! in the server process -- the sandboxed runtime is never involved.
//!
//! Mirrors the `plugins` module's store/service split and shares the config
//! store's SqlitePool.

mod prompt;
mod service;
mod store;
mod toolbox;

pub use prompt::render_index;
pub use service::MemoryService;
pub use store::{MemoryRow, MemorySpaceRow, MemoryStore};
pub use toolbox::MemoryToolbox;

/// Cap on a memory's one-line description. The index ships every description
/// in the system prompt on every turn, so this bounds the fixed per-turn cost.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// Cap on how many memories the rendered index lists before truncating.
pub const MAX_INDEX_ENTRIES: usize = 200;

/// Space and memory names are slugs. Rejecting `/` is what keeps the
/// `space/name` address the agent uses unambiguous.
pub fn validate_slug(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if s.chars().count() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    let first = s.chars().next().unwrap_or('-');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "name '{s}' must start with a lowercase letter or digit"
        ));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "name '{s}' may only contain lowercase letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}
```

Note: `prompt`, `service`, and `toolbox` are declared here but created in Tasks 2 and 4. To keep this task compiling on its own, comment out the `mod prompt; mod service; mod toolbox;` lines and their `pub use`s, with a `// added in Task 2/4` marker, and uncomment them in those tasks.

Add `pub mod memory;` to `server/src/lib.rs`, alphabetically among the existing `pub mod` lines.

- [ ] **Step 3: Write the failing store tests**

Append to `server/src/memory/store.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn store() -> (MemoryStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (MemoryStore::new(pool), tmp)
    }

    fn mem(space: &str, name: &str) -> MemoryRow {
        MemoryRow {
            id: 0,
            space: space.into(),
            name: name.into(),
            description: "d".into(),
            content: "body".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn migration_seeds_exactly_the_default_space() {
        let (s, _t) = store().await;
        let spaces = s.list_spaces().await.unwrap();
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "default");
    }

    #[tokio::test]
    async fn memory_crud_roundtrip() {
        let (s, _t) = store().await;
        let id = s.create_memory(&mem("default", "alpha")).await.unwrap();
        let got = s.get_memory(id).await.unwrap().unwrap();
        assert_eq!(got.name, "alpha");
        assert_eq!(got.content, "body");

        let by_ref = s.get_memory_by_ref("default", "alpha").await.unwrap();
        assert_eq!(by_ref.unwrap().id, id);

        assert!(s.update_memory(id, None, Some("new body"), "2").await.unwrap());
        let got = s.get_memory(id).await.unwrap().unwrap();
        assert_eq!(got.content, "new body");
        assert_eq!(got.description, "d", "None must leave the field untouched");
        assert_eq!(got.updated_at, "2");

        assert!(s.delete_memory(id).await.unwrap());
        assert!(s.get_memory(id).await.unwrap().is_none());
        assert!(!s.delete_memory(id).await.unwrap(), "second delete is a miss");
    }

    #[tokio::test]
    async fn duplicate_name_in_same_space_is_rejected_but_allowed_across_spaces() {
        let (s, _t) = store().await;
        s.create_space(&MemorySpaceRow {
            name: "other".into(),
            description: String::new(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .await
        .unwrap();
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        assert!(s.create_memory(&mem("default", "alpha")).await.is_err());
        s.create_memory(&mem("other", "alpha")).await.unwrap();
    }

    #[tokio::test]
    async fn create_memory_in_unknown_space_is_rejected() {
        let (s, _t) = store().await;
        let err = s.create_memory(&mem("nope", "alpha")).await.unwrap_err();
        assert!(err.contains("nope"), "error should name the space: {err}");
    }

    #[tokio::test]
    async fn deleting_a_space_deletes_its_memories_only() {
        let (s, _t) = store().await;
        s.create_space(&MemorySpaceRow {
            name: "other".into(),
            description: String::new(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .await
        .unwrap();
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        s.create_memory(&mem("other", "beta")).await.unwrap();

        assert!(s.delete_space("default").await.unwrap());
        assert!(s.get_space("default").await.unwrap().is_none());
        assert!(s.list_memories(Some("default")).await.unwrap().is_empty());
        assert_eq!(s.list_memories(Some("other")).await.unwrap().len(), 1);
        assert!(!s.delete_space("default").await.unwrap());
    }

    #[tokio::test]
    async fn renaming_a_space_carries_its_memories_and_orphans_none() {
        let (s, _t) = store().await;
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        s.rename_space("default", "renamed", "2").await.unwrap();

        assert!(s.get_space("default").await.unwrap().is_none());
        let moved = s.list_memories(Some("renamed")).await.unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].name, "alpha");
        assert!(s.list_memories(Some("default")).await.unwrap().is_empty());
        // Nothing left pointing at a space that no longer exists.
        let all = s.list_memories(None).await.unwrap();
        assert!(all.iter().all(|m| m.space == "renamed"));
    }

    #[tokio::test]
    async fn rename_onto_an_existing_space_is_rejected_and_changes_nothing() {
        let (s, _t) = store().await;
        s.create_space(&MemorySpaceRow {
            name: "other".into(),
            description: String::new(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .await
        .unwrap();
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        assert!(s.rename_space("default", "other", "2").await.is_err());
        assert!(s.get_space("default").await.unwrap().is_some());
        assert_eq!(s.list_memories(Some("default")).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memories_in_filters_to_the_named_spaces_and_orders_stably() {
        let (s, _t) = store().await;
        for name in ["other", "third"] {
            s.create_space(&MemorySpaceRow {
                name: name.into(),
                description: String::new(),
                created_at: "1".into(),
                updated_at: "1".into(),
            })
            .await
            .unwrap();
        }
        s.create_memory(&mem("other", "b")).await.unwrap();
        s.create_memory(&mem("default", "a")).await.unwrap();
        s.create_memory(&mem("third", "c")).await.unwrap();

        let rows = s
            .memories_in(&["default".to_string(), "other".to_string()])
            .await
            .unwrap();
        let got: Vec<_> = rows.iter().map(|r| (r.space.as_str(), r.name.as_str())).collect();
        assert_eq!(got, vec![("default", "a"), ("other", "b")]);

        assert!(s.memories_in(&[]).await.unwrap().is_empty());
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p horsie-server memory::store -- --nocapture`
Expected: FAIL to compile — `MemoryStore` and the row types do not exist yet.

- [ ] **Step 5: Implement `server/src/memory/store.rs`**

```rust
//! SQLite storage for memory spaces and memories, sharing the config store's
//! pool. No secrets, so this is a plain metadata store (mirrors
//! `plugins::store` without the artifact bookkeeping).
//!
//! `memories.space` is not a SQL foreign key -- see `0008_memory.sql` for why.
//! The relationship is enforced here: `create_memory` checks the space exists,
//! and `delete_space` / `rename_space` fix up children inside a transaction.

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

const SPACE_COLS: &str = "name, description, created_at, updated_at";
const MEMORY_COLS: &str = "id, space, name, description, content, created_at, updated_at";

/// One row of the `memory_spaces` table.
#[derive(Clone, Debug, PartialEq)]
pub struct MemorySpaceRow {
    pub name: String,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One row of the `memories` table. `id` is ignored on insert (the column is
/// AUTOINCREMENT); `create_memory` returns the assigned id.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRow {
    pub id: i64,
    pub space: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MemoryStore {
    pool: SqlitePool,
}

impl MemoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // --- spaces ---

    pub async fn list_spaces(&self) -> Result<Vec<MemorySpaceRow>, String> {
        let rows = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_space).collect()
    }

    pub async fn get_space(&self, name: &str) -> Result<Option<MemorySpaceRow>, String> {
        let row = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_space).transpose()
    }

    /// Insert a space. Errs when the name is taken (no upsert: a silent
    /// overwrite would discard the existing description).
    pub async fn create_space(&self, row: &MemorySpaceRow) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO memory_spaces (name, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create space '{}': {e}", row.name))?;
        Ok(())
    }

    /// Returns false when no space by that name exists.
    pub async fn update_space_description(
        &self,
        name: &str,
        description: &str,
        updated_at: &str,
    ) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE memory_spaces SET description = ?, updated_at = ? WHERE name = ?",
        )
        .bind(description)
        .bind(updated_at)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    /// Rename a space, carrying its memories across. The space name is the join
    /// key, so a bare `UPDATE memory_spaces SET name = ?` would orphan every
    /// memory in it -- all three statements run in one transaction.
    pub async fn rename_space(
        &self,
        old: &str,
        new: &str,
        updated_at: &str,
    ) -> Result<(), String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let existing = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces WHERE name = ?"
        ))
        .bind(old)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        let existing = existing
            .as_ref()
            .map(row_to_space)
            .transpose()?
            .ok_or_else(|| format!("unknown memory space '{old}'"))?;

        sqlx::query(
            "INSERT INTO memory_spaces (name, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(new)
        .bind(&existing.description)
        .bind(&existing.created_at)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("rename to '{new}': {e}"))?;

        sqlx::query("UPDATE memories SET space = ? WHERE space = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

        sqlx::query("DELETE FROM memory_spaces WHERE name = ?")
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Delete a space and every memory in it. Returns false when the space did
    /// not exist.
    pub async fn delete_space(&self, name: &str) -> Result<bool, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM memories WHERE space = ?")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let res = sqlx::query("DELETE FROM memory_spaces WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    // --- memories ---

    /// All memories, or just one space's, ordered by `space, name`.
    pub async fn list_memories(&self, space: Option<&str>) -> Result<Vec<MemoryRow>, String> {
        let rows = match space {
            Some(s) => {
                sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM memories WHERE space = ? ORDER BY space, name"
                ))
                .bind(s)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM memories ORDER BY space, name"
                ))
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_memory).collect()
    }

    /// Memories across several spaces -- the index query, run once per turn.
    /// An empty `spaces` yields an empty result without touching the DB.
    pub async fn memories_in(&self, spaces: &[String]) -> Result<Vec<MemoryRow>, String> {
        if spaces.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; spaces.len()].join(", ");
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE space IN ({placeholders}) \
             ORDER BY space, name"
        );
        let mut q = sqlx::query(&sql);
        for s in spaces {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(|e| e.to_string())?;
        rows.iter().map(row_to_memory).collect()
    }

    pub async fn get_memory(&self, id: i64) -> Result<Option<MemoryRow>, String> {
        let row = sqlx::query(&format!("SELECT {MEMORY_COLS} FROM memories WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_memory).transpose()
    }

    pub async fn get_memory_by_ref(
        &self,
        space: &str,
        name: &str,
    ) -> Result<Option<MemoryRow>, String> {
        let row = sqlx::query(&format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE space = ? AND name = ?"
        ))
        .bind(space)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_memory).transpose()
    }

    /// Insert a memory, returning its assigned id. Verifies the space exists in
    /// the same transaction as the insert, since there is no FK to do it.
    pub async fn create_memory(&self, row: &MemoryRow) -> Result<i64, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let space = sqlx::query("SELECT name FROM memory_spaces WHERE name = ?")
            .bind(&row.space)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if space.is_none() {
            return Err(format!("unknown memory space '{}'", row.space));
        }
        let res = sqlx::query(
            "INSERT INTO memories (space, name, description, content, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.space)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.content)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("create memory '{}/{}': {e}", row.space, row.name))?;
        let id = res.last_insert_rowid();
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Update the supplied fields only; `None` leaves a field untouched.
    /// Returns false when no memory has that id.
    pub async fn update_memory(
        &self,
        id: i64,
        description: Option<&str>,
        content: Option<&str>,
        updated_at: &str,
    ) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE memories SET description = COALESCE(?, description), \
             content = COALESCE(?, content), updated_at = ? WHERE id = ?",
        )
        .bind(description)
        .bind(content)
        .bind(updated_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete_memory(&self, id: i64) -> Result<bool, String> {
        let res = sqlx::query("DELETE FROM memories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_space(row: &SqliteRow) -> Result<MemorySpaceRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(MemorySpaceRow {
        name: get_s("name")?,
        description: get_s("description")?,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

fn row_to_memory(row: &SqliteRow) -> Result<MemoryRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(MemoryRow {
        id: row.try_get::<i64, _>("id").map_err(|e| e.to_string())?,
        space: get_s("space")?,
        name: get_s("name")?,
        description: get_s("description")?,
        content: get_s("content")?,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p horsie-server memory::store`
Expected: PASS, 8 tests.

If `renaming_a_space_carries_its_memories_and_orphans_none` fails, the transaction ordering is wrong: the new space row must be inserted **before** the `UPDATE memories`, or the update points children at a space that does not exist yet.

- [ ] **Step 7: Write and run slug validation tests**

Append to `server/src/memory/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::validate_slug;

    #[test]
    fn accepts_lowercase_slugs() {
        for s in ["a", "default", "my-space", "repo.name_2", "9lives"] {
            assert!(validate_slug(s).is_ok(), "{s} should be valid");
        }
    }

    #[test]
    fn rejects_slashes_uppercase_spaces_and_empty() {
        for s in ["", "Has-Upper", "has space", "a/b", "-leading", ".dot"] {
            assert!(validate_slug(s).is_err(), "{s} should be invalid");
        }
    }

    #[test]
    fn rejects_overlong_names() {
        assert!(validate_slug(&"a".repeat(65)).is_err());
        assert!(validate_slug(&"a".repeat(64)).is_ok());
    }
}
```

Run: `cargo test -p horsie-server memory::`
Expected: PASS, 11 tests.

- [ ] **Step 8: Commit**

```bash
git add server/migrations/0008_memory.sql server/src/memory/ server/src/lib.rs
git commit -m "memory: schema and store for memory spaces and memories"
```

---

### Task 2: Wire types, `MemoryService`, and the prompt index renderer

**Files:**
- Create: `models/fluorite/memory.fl`
- Create: `server/src/memory/service.rs`
- Create: `server/src/memory/prompt.rs`
- Modify: `models/src/lib.rs`
- Modify: `server/src/memory/mod.rs` (uncomment `mod service; mod prompt;` and their re-exports)
- Test: in-file `mod tests` in both new files

**Interfaces:**
- Consumes: `MemoryStore` and its row types, `validate_slug`, `MAX_DESCRIPTION_CHARS`, `MAX_INDEX_ENTRIES` from Task 1.
- Produces: `MemoryService::new(MemoryStore)`; `list_spaces() -> Result<Vec<MemorySpaceView>, String>`, `create_space(MemorySpaceCreateInput) -> Result<MemorySpaceView, String>`, `update_space(&str, MemorySpaceUpdateInput) -> Result<MemorySpaceView, String>`, `delete_space(&str) -> Result<(), String>`, `list_memories(Option<&str>) -> Result<Vec<MemoryView>, String>`, `get_memory(i64) -> Result<MemoryView, String>`, `create_memory(MemoryCreateInput) -> Result<MemoryView, String>`, `update_memory(i64, MemoryUpdateInput) -> Result<MemoryView, String>`, `delete_memory(i64) -> Result<(), String>`, and the agent-facing `memories_in(&[String]) -> Result<Vec<MemoryRow>, String>` (used by both the prompt index and `memory_list`) and `get_by_ref(&str, &str) -> Result<Option<MemoryRow>, String>` (used by `memory_load` and to resolve update/delete addresses). Plus `render_index(&[MemoryRow], &[String]) -> String`.
- Wire types (fluorite package `memory`): `MemorySpaceView`, `MemorySpaceCreateInput`, `MemorySpaceUpdateInput`, `MemoryView`, `MemoryCreateInput`, `MemoryUpdateInput`.

- [ ] **Step 1: Write `models/fluorite/memory.fl`**

```
/// Wire contracts for agent-managed long-term memories. A memory space is a
/// named namespace holding a flat set of memories; sessions select spaces at
/// creation and the agent manages their contents with the memory_* tools.
package memory;

/// A memory space as shown in the web UI.
struct MemorySpaceView {
    name: String,
    description: String,
    /// How many memories the space holds.
    memory_count: u32,
}

/// Create a memory space. `name` must be a slug: lowercase letters, digits,
/// '.', '_' and '-', starting with a letter or digit.
struct MemorySpaceCreateInput {
    name: String,
    description: Option<String>,
}

/// Rename a space and/or change its description. Omitted fields are unchanged.
/// Renaming carries the space's memories across.
struct MemorySpaceUpdateInput {
    name: Option<String>,
    description: Option<String>,
}

/// One memory, body included. Addressed by the agent as `<space>/<name>`.
struct MemoryView {
    id: u64,
    space: String,
    name: String,
    /// One line, shown in the agent's prompt index.
    description: String,
    /// Markdown body, loaded on demand.
    content: String,
    /// Unix epoch seconds.
    created_at: String,
    updated_at: String,
}

struct MemoryCreateInput {
    space: String,
    name: String,
    description: String,
    content: String,
}

/// Omitted fields are left unchanged; supplied ones are replaced wholesale.
struct MemoryUpdateInput {
    description: Option<String>,
    content: Option<String>,
}
```

- [ ] **Step 2: Add the module include to `models/src/lib.rs`**

`models/build.rs` globs the `fluorite/` directory, so no build script change is needed. Add, next to the existing `plugins` block:

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod memory {
    include!(concat!(env!("OUT_DIR"), "/memory/mod.rs"));
}
```

Run: `cargo build -p horsie-models`
Expected: builds; `horsie_models::memory::MemoryView` now exists.

- [ ] **Step 3: Write the failing prompt-renderer tests**

Create `server/src/memory/prompt.rs` with only this test module for now:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::render_index;
    use crate::memory::MemoryRow;

    fn row(space: &str, name: &str, description: &str) -> MemoryRow {
        MemoryRow {
            id: 1,
            space: space.into(),
            name: name.into(),
            description: description.into(),
            content: "body".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn groups_by_space_and_uses_qualified_addresses() {
        let rows = vec![
            row("default", "alpha", "first fact"),
            row("default", "beta", "second fact"),
            row("ops", "gamma", "third fact"),
        ];
        let out = render_index(&rows, &["default".into(), "ops".into()]);
        assert!(out.starts_with("# Memories\n"));
        assert!(out.contains("## default\n"));
        assert!(out.contains("- default/alpha — first fact\n"));
        assert!(out.contains("- default/beta — second fact\n"));
        assert!(out.contains("## ops\n"));
        assert!(out.contains("- ops/gamma — third fact\n"));
        assert!(out.contains("memory_load"), "must tell the agent how to read one");
    }

    #[test]
    fn empty_spaces_still_render_so_the_agent_knows_memory_exists() {
        let out = render_index(&[], &["default".into()]);
        assert!(out.contains("# Memories"));
        assert!(out.contains("No memories saved yet"));
        assert!(out.contains("default"), "must name the writable spaces");
    }

    #[test]
    fn no_selected_spaces_renders_nothing() {
        assert_eq!(render_index(&[], &[]), "");
    }

    #[test]
    fn truncation_is_announced_not_silent() {
        let rows: Vec<MemoryRow> = (0..250)
            .map(|i| row("default", &format!("m{i:03}"), "d"))
            .collect();
        let out = render_index(&rows, &["default".into()]);
        assert!(out.contains("50 more memories not listed"));
        assert_eq!(out.matches("- default/m").count(), 200);
    }

    #[test]
    fn newlines_in_a_description_cannot_break_the_index_layout() {
        let rows = vec![row("default", "alpha", "line one\nline two")];
        let out = render_index(&rows, &["default".into()]);
        assert!(out.contains("- default/alpha — line one line two\n"));
    }
}
```

- [ ] **Step 4: Run to verify failure**

Run: `cargo test -p horsie-server memory::prompt`
Expected: FAIL to compile — `render_index` does not exist.

- [ ] **Step 5: Implement `render_index` above that test module**

```rust
//! Renders the memory index that rides in the session system prompt: one line
//! per memory, grouped by space. Bodies are never inlined -- the agent pulls
//! the ones it wants with `memory_load`. Pure and synchronous so it is cheap to
//! test; the DB read happens in the caller.

use crate::memory::{MAX_INDEX_ENTRIES, MemoryRow};

/// Build the `# Memories` prompt section for a session's selected `spaces`.
/// Returns an empty string when the session selected no spaces at all -- in
/// that case the memory tools are not exposed either, so the section would be
/// noise. When spaces are selected but hold nothing, the section still renders:
/// the agent needs to know the facility exists before it can use it.
pub fn render_index(rows: &[MemoryRow], spaces: &[String]) -> String {
    if spaces.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Memories\n\nSaved notes from earlier sessions. Each line is one memory: \
         its address, then a one-line summary. Load the full text of the ones you \
         need with the memory_load tool before relying on them.\n",
    );
    if rows.is_empty() {
        out.push_str(&format!(
            "\nNo memories saved yet. Writable spaces: {}.\n",
            spaces.join(", ")
        ));
        return out;
    }

    let shown = rows.len().min(MAX_INDEX_ENTRIES);
    let mut current: Option<&str> = None;
    for row in rows.iter().take(shown) {
        if current != Some(row.space.as_str()) {
            out.push_str(&format!("\n## {}\n\n", row.space));
            current = Some(row.space.as_str());
        }
        out.push_str(&format!(
            "- {}/{} — {}\n",
            row.space,
            row.name,
            one_line(&row.description)
        ));
    }
    if rows.len() > shown {
        out.push_str(&format!(
            "\n({} more memories not listed — use memory_list to see the rest.)\n",
            rows.len() - shown
        ));
    }
    out
}

/// Collapse any whitespace run to a single space so one description can never
/// occupy more than one index line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
```

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p horsie-server memory::prompt`
Expected: PASS, 5 tests.

- [ ] **Step 7: Write the failing service tests**

Create `server/src/memory/service.rs` with this test module (implementation follows in Step 9):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn service() -> (MemoryService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (MemoryService::new(MemoryStore::new(pool)), tmp)
    }

    fn create(space: &str, name: &str) -> MemoryCreateInput {
        MemoryCreateInput {
            space: space.into(),
            name: name.into(),
            description: "a fact".into(),
            content: "the body".into(),
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_an_id_and_timestamps() {
        let (s, _t) = service().await;
        let v = s.create_memory(create("default", "alpha")).await.unwrap();
        assert!(v.id > 0);
        assert_eq!(v.space, "default");
        assert_eq!(v.content, "the body");
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn rejects_invalid_slugs_and_overlong_descriptions() {
        let (s, _t) = service().await;
        let mut bad = create("default", "Not A Slug");
        assert!(s.create_memory(bad.clone()).await.is_err());

        bad = create("default", "alpha");
        bad.description = "x".repeat(crate::memory::MAX_DESCRIPTION_CHARS + 1);
        let err = s.create_memory(bad).await.unwrap_err();
        assert!(err.contains("description"), "{err}");
    }

    #[tokio::test]
    async fn update_replaces_only_supplied_fields_and_bumps_updated_at() {
        let (s, _t) = service().await;
        let v = s.create_memory(create("default", "alpha")).await.unwrap();
        let updated = s
            .update_memory(
                v.id,
                MemoryUpdateInput {
                    description: None,
                    content: Some("new body".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.description, "a fact");
        assert_eq!(updated.content, "new body");
        assert_eq!(updated.created_at, v.created_at);
    }

    #[tokio::test]
    async fn missing_memory_errors_on_get_update_and_delete() {
        let (s, _t) = service().await;
        assert!(s.get_memory(999).await.is_err());
        assert!(
            s.update_memory(
                999,
                MemoryUpdateInput { description: None, content: Some("x".into()) }
            )
            .await
            .is_err()
        );
        assert!(s.delete_memory(999).await.is_err());
    }

    #[tokio::test]
    async fn space_views_carry_a_memory_count() {
        let (s, _t) = service().await;
        s.create_memory(create("default", "alpha")).await.unwrap();
        s.create_memory(create("default", "beta")).await.unwrap();
        let spaces = s.list_spaces().await.unwrap();
        let d = spaces.iter().find(|x| x.name == "default").unwrap();
        assert_eq!(d.memory_count, 2);
    }

    #[tokio::test]
    async fn create_space_validates_and_rejects_duplicates() {
        let (s, _t) = service().await;
        assert!(
            s.create_space(MemorySpaceCreateInput {
                name: "Bad Name".into(),
                description: None,
            })
            .await
            .is_err()
        );
        s.create_space(MemorySpaceCreateInput {
            name: "ops".into(),
            description: Some("operational facts".into()),
        })
        .await
        .unwrap();
        assert!(
            s.create_space(MemorySpaceCreateInput {
                name: "ops".into(),
                description: None,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn update_space_can_rename_and_carries_memories() {
        let (s, _t) = service().await;
        s.create_memory(create("default", "alpha")).await.unwrap();
        let v = s
            .update_space(
                "default",
                MemorySpaceUpdateInput {
                    name: Some("renamed".into()),
                    description: Some("moved".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(v.name, "renamed");
        assert_eq!(v.description, "moved");
        assert_eq!(v.memory_count, 1);
    }

    #[tokio::test]
    async fn deleting_a_missing_space_errors() {
        let (s, _t) = service().await;
        assert!(s.delete_space("nope").await.is_err());
    }
}
```

`MemoryCreateInput` must derive `Clone` for the `bad.clone()` above — fluorite already emits `Clone` on every struct (see `models/build.rs` derives), so nothing to add.

- [ ] **Step 8: Run to verify failure**

Run: `cargo test -p horsie-server memory::service`
Expected: FAIL to compile — `MemoryService` does not exist.

- [ ] **Step 9: Implement `MemoryService` above that test module**

```rust
//! Validation, timestamps, and row→wire mapping over `MemoryStore`. Also the
//! agent-facing reads (`memories_in`, `get_by_ref`) the toolbox and the prompt
//! index use, so the session layer never touches the store directly.

use crate::memory::{MAX_DESCRIPTION_CHARS, MemoryRow, MemorySpaceRow, MemoryStore, validate_slug};
use horsie_models::memory::{
    MemoryCreateInput, MemorySpaceCreateInput, MemorySpaceUpdateInput, MemorySpaceView,
    MemoryUpdateInput, MemoryView,
};

pub struct MemoryService {
    store: MemoryStore,
}

impl MemoryService {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }

    // --- spaces ---

    pub async fn list_spaces(&self) -> Result<Vec<MemorySpaceView>, String> {
        let spaces = self.store.list_spaces().await?;
        let all = self.store.list_memories(None).await?;
        Ok(spaces
            .into_iter()
            .map(|s| {
                let count = all.iter().filter(|m| m.space == s.name).count();
                space_view(&s, count)
            })
            .collect())
    }

    pub async fn create_space(
        &self,
        input: MemorySpaceCreateInput,
    ) -> Result<MemorySpaceView, String> {
        validate_slug(&input.name)?;
        let now = now_secs();
        self.store
            .create_space(&MemorySpaceRow {
                name: input.name.clone(),
                description: input.description.unwrap_or_default(),
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;
        self.space_view_of(&input.name).await
    }

    /// Rename and/or re-describe. The rename runs first so the description
    /// update lands on the new row.
    pub async fn update_space(
        &self,
        name: &str,
        input: MemorySpaceUpdateInput,
    ) -> Result<MemorySpaceView, String> {
        if self.store.get_space(name).await?.is_none() {
            return Err(format!("unknown memory space '{name}'"));
        }
        let now = now_secs();
        let mut current = name.to_string();
        if let Some(new_name) = input.name {
            if new_name != current {
                validate_slug(&new_name)?;
                self.store.rename_space(&current, &new_name, &now).await?;
                current = new_name;
            }
        }
        if let Some(description) = input.description {
            self.store
                .update_space_description(&current, &description, &now)
                .await?;
        }
        self.space_view_of(&current).await
    }

    pub async fn delete_space(&self, name: &str) -> Result<(), String> {
        if self.store.delete_space(name).await? {
            Ok(())
        } else {
            Err(format!("unknown memory space '{name}'"))
        }
    }

    async fn space_view_of(&self, name: &str) -> Result<MemorySpaceView, String> {
        let row = self
            .store
            .get_space(name)
            .await?
            .ok_or_else(|| format!("unknown memory space '{name}'"))?;
        let count = self.store.list_memories(Some(name)).await?.len();
        Ok(space_view(&row, count))
    }

    // --- memories ---

    pub async fn list_memories(&self, space: Option<&str>) -> Result<Vec<MemoryView>, String> {
        Ok(self
            .store
            .list_memories(space)
            .await?
            .iter()
            .map(memory_view)
            .collect())
    }

    pub async fn get_memory(&self, id: i64) -> Result<MemoryView, String> {
        self.store
            .get_memory(id)
            .await?
            .as_ref()
            .map(memory_view)
            .ok_or_else(|| format!("no memory with id {id}"))
    }

    pub async fn create_memory(&self, input: MemoryCreateInput) -> Result<MemoryView, String> {
        validate_slug(&input.space)?;
        validate_slug(&input.name)?;
        check_description(&input.description)?;
        let now = now_secs();
        let id = self
            .store
            .create_memory(&MemoryRow {
                id: 0,
                space: input.space,
                name: input.name,
                description: input.description,
                content: input.content,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;
        self.get_memory(id).await
    }

    pub async fn update_memory(
        &self,
        id: i64,
        input: MemoryUpdateInput,
    ) -> Result<MemoryView, String> {
        if let Some(d) = input.description.as_deref() {
            check_description(d)?;
        }
        let changed = self
            .store
            .update_memory(
                id,
                input.description.as_deref(),
                input.content.as_deref(),
                &now_secs(),
            )
            .await?;
        if !changed {
            return Err(format!("no memory with id {id}"));
        }
        self.get_memory(id).await
    }

    pub async fn delete_memory(&self, id: i64) -> Result<(), String> {
        if self.store.delete_memory(id).await? {
            Ok(())
        } else {
            Err(format!("no memory with id {id}"))
        }
    }

    // --- agent-facing reads ---

    /// Rows across a session's selected spaces, for the prompt index and
    /// `memory_list`.
    pub async fn memories_in(&self, spaces: &[String]) -> Result<Vec<MemoryRow>, String> {
        self.store.memories_in(spaces).await
    }

    /// Resolve a `space/name` address.
    pub async fn get_by_ref(&self, space: &str, name: &str) -> Result<Option<MemoryRow>, String> {
        self.store.get_memory_by_ref(space, name).await
    }
}

fn check_description(d: &str) -> Result<(), String> {
    if d.trim().is_empty() {
        return Err("description must not be empty".to_string());
    }
    if d.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters (got {})",
            d.chars().count()
        ));
    }
    Ok(())
}

fn space_view(row: &MemorySpaceRow, memory_count: usize) -> MemorySpaceView {
    MemorySpaceView {
        name: row.name.clone(),
        description: row.description.clone(),
        memory_count: u32::try_from(memory_count).unwrap_or(u32::MAX),
    }
}

fn memory_view(row: &MemoryRow) -> MemoryView {
    MemoryView {
        id: u64::try_from(row.id).unwrap_or(0),
        space: row.space.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        content: row.content.clone(),
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

- [ ] **Step 10: Uncomment the module declarations and run the whole module**

In `server/src/memory/mod.rs`, uncomment `mod prompt;`, `mod service;`, and the `pub use prompt::render_index;` / `pub use service::MemoryService;` lines (leave `toolbox` commented — Task 4).

Run: `cargo test -p horsie-server memory::`
Expected: PASS, 24 tests.

- [ ] **Step 11: Commit**

```bash
git add models/fluorite/memory.fl models/src/lib.rs server/src/memory/
git commit -m "memory: wire types, service layer, and prompt index renderer"
```

---

### Task 3: HTTP routes

**Files:**
- Create: `server/src/http/memory.rs`
- Modify: `server/src/http/mod.rs` (module decl, `AppState.memory`, routes, `test_state`, plus a new integration test)
- Modify: `server/src/bin/horsie-server/main.rs` (construct the service, put it in `AppState`)

**Interfaces:**
- Consumes: `MemoryService` and the `horsie_models::memory` wire types from Task 2.
- Produces: `AppState.memory: Arc<crate::memory::MemoryService>`; routes `/api/memory-spaces`, `/api/memory-spaces/:name`, `/api/memories`, `/api/memories/:id`.

- [ ] **Step 1: Write the failing HTTP integration test**

Add to the `mod tests` block in `server/src/http/mod.rs`, following the shape of the existing `plugins_install_list_artifact_delete_over_http` test (copy its request/response helpers rather than inventing new ones):

```rust
#[tokio::test]
async fn memory_spaces_and_memories_crud_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app(test_state(&tmp).await);

    // The migration seeds exactly one space.
    let (status, body) = get_json(&app, "/api/memory-spaces").await;
    assert_eq!(status, StatusCode::OK);
    let spaces = body.as_array().unwrap();
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0]["name"], "default");
    assert_eq!(spaces[0]["memoryCount"], 0);

    // Create a memory in it.
    let (status, body) = post_json(
        &app,
        "/api/memories",
        json!({
            "space": "default",
            "name": "alpha",
            "description": "a durable fact",
            "content": "the body"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_u64().unwrap();
    assert_eq!(body["space"], "default");

    // It shows up in the listing, and the space's count follows.
    let (_, body) = get_json(&app, "/api/memories?space=default").await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (_, body) = get_json(&app, "/api/memory-spaces").await;
    assert_eq!(body[0]["memoryCount"], 1);

    // Update only the content.
    let (status, body) = put_json(
        &app,
        &format!("/api/memories/{id}"),
        json!({ "content": "new body" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "new body");
    assert_eq!(body["description"], "a durable fact");

    // A bad slug is a 422, not a 500.
    let (status, _) = post_json(
        &app,
        "/api/memories",
        json!({"space": "default", "name": "Bad Name", "description": "d", "content": "c"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A missing memory is a 404.
    let (status, _) = get_json(&app, "/api/memories/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Deleting the space takes its memories with it.
    let (status, _) = delete(&app, "/api/memory-spaces/default").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = get_json(&app, "/api/memories").await;
    assert!(body.as_array().unwrap().is_empty());
}
```

Note the JSON keys are **camelCase** (`memoryCount`) — fluorite's TS/serde convention in this repo. Confirm against the generated Rust struct's serde attributes when the test first runs; if it emits snake_case, match what the generator actually produces rather than forcing a rename.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server memory_spaces_and_memories_crud_over_http`
Expected: FAIL to compile — `AppState` has no `memory` field and the routes do not exist.

- [ ] **Step 3: Write `server/src/http/memory.rs`**

```rust
//! HTTP surface for agent memory: CRUD over memory spaces and the memories in
//! them, for the web UI. The agent reaches the same data through
//! `MemoryToolbox`, not through these routes.

use super::AppState;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use horsie_models::memory::{
    MemoryCreateInput, MemorySpaceCreateInput, MemorySpaceUpdateInput, MemorySpaceView,
    MemoryUpdateInput, MemoryView,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    space: Option<String>,
}

/// GET /api/memory-spaces
pub async fn list_spaces(State(state): State<AppState>) -> Result<Json<Vec<MemorySpaceView>>, Api> {
    state
        .memory
        .list_spaces()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// POST /api/memory-spaces
pub async fn create_space(
    State(state): State<AppState>,
    Json(input): Json<MemorySpaceCreateInput>,
) -> Result<(StatusCode, Json<MemorySpaceView>), Api> {
    state
        .memory
        .create_space(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// PUT /api/memory-spaces/:name — rename and/or re-describe.
pub async fn update_space(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<MemorySpaceUpdateInput>,
) -> Result<Json<MemorySpaceView>, Api> {
    state
        .memory
        .update_space(&name, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/memory-spaces/:name — removes the space and its memories.
pub async fn delete_space(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .memory
        .delete_space(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::not_found)
}

/// GET /api/memories?space=<name>
pub async fn list_memories(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MemoryView>>, Api> {
    state
        .memory
        .list_memories(q.space.as_deref())
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// GET /api/memories/:id
pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<MemoryView>, Api> {
    state
        .memory
        .get_memory(id)
        .await
        .map(Json)
        .map_err(Api::not_found)
}

/// POST /api/memories
pub async fn create_memory(
    State(state): State<AppState>,
    Json(input): Json<MemoryCreateInput>,
) -> Result<(StatusCode, Json<MemoryView>), Api> {
    state
        .memory
        .create_memory(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// PUT /api/memories/:id
pub async fn update_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<MemoryUpdateInput>,
) -> Result<Json<MemoryView>, Api> {
    state
        .memory
        .update_memory(id, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/memories/:id
pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Api> {
    state
        .memory
        .delete_memory(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::not_found)
}
```

`update_memory` maps a missing id to 422 rather than 404 because the service returns one error type for both "not found" and "bad description". If the integration test's 404 expectation for `GET /api/memories/99999` passes but a missing-id `PUT` reads wrong to you later, split the service error then — not now.

- [ ] **Step 4: Wire it into `server/src/http/mod.rs`**

Add `mod memory;` next to the existing `mod plugins;`. Add the field to `AppState`, after `plugins`:

```rust
    /// Agent-managed long-term memories: CRUD for the web UI. The agent reaches
    /// the same data through its `MemoryToolbox`, not over HTTP.
    pub memory: Arc<crate::memory::MemoryService>,
```

Add the routes in `app()`, after the `/api/plugin-artifacts/:file` line:

```rust
        .route(
            "/api/memory-spaces",
            get(memory::list_spaces).post(memory::create_space),
        )
        .route(
            "/api/memory-spaces/:name",
            put(memory::update_space).delete(memory::delete_space),
        )
        .route(
            "/api/memories",
            get(memory::list_memories).post(memory::create_memory),
        )
        .route(
            "/api/memories/:id",
            get(memory::get_memory)
                .put(memory::update_memory)
                .delete(memory::delete_memory),
        )
```

In `test_state`, next to the `plugins` construction:

```rust
    let memory = Arc::new(crate::memory::MemoryService::new(
        crate::memory::MemoryStore::new(opened.pool.clone()),
    ));
```

and add `memory,` to the returned `AppState`.

- [ ] **Step 5: Wire it into the binary**

In `server/src/bin/horsie-server/main.rs`, alongside the existing `github` / `mcp` / `plugins` service construction (they all share `opened.pool.clone()`):

```rust
    let memory = Arc::new(horsie_server::memory::MemoryService::new(
        horsie_server::memory::MemoryStore::new(opened.pool.clone()),
    ));
```

Add `memory: memory.clone(),` to the `AppState` literal. (`ServerDeps` gets its own copy in Task 5 — leave it alone here.)

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p horsie-server memory`
Expected: PASS — the Task 1/2 unit tests plus `memory_spaces_and_memories_crud_over_http`.

- [ ] **Step 7: Commit**

```bash
git add server/src/http/memory.rs server/src/http/mod.rs server/src/bin/horsie-server/main.rs
git commit -m "memory: HTTP CRUD for spaces and memories"
```

---

### Task 4: `MemoryToolbox` — the five agent tools

**Files:**
- Create: `server/src/memory/toolbox.rs`
- Modify: `server/src/memory/mod.rs` (uncomment `mod toolbox;` and `pub use toolbox::MemoryToolbox;`)
- Test: in-file `mod tests` in `server/src/memory/toolbox.rs`

**Interfaces:**
- Consumes: `MemoryService` (Task 2), `horsie_agentcore::{Tool, ToolSpec, Toolbox}`, `horsie_agentcore::error::ToolCallError`.
- Produces: `MemoryToolbox::new(inner: Arc<dyn Toolbox>, service: Arc<MemoryService>, spaces: Vec<String>) -> MemoryToolbox`, implementing `Toolbox`. Tool names: `memory_load`, `memory_create`, `memory_update`, `memory_delete`, `memory_list`.

**Why a wrapper, not a composed box:** anything passed through `DefaultToolboxFactory::for_agent`'s `mcp` parameter is wrapped in `FilteredToolbox` when the session sets `allowed_tools`, so memory tools would silently vanish for such sessions. Wrapping outside the factory — exactly as `AskUserToolbox` does at `server/src/sessions/session_actor.rs:888` — makes space selection the single gate.

- [ ] **Step 1: Write the failing toolbox tests**

Create `server/src/memory/toolbox.rs` with this test module:

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
    use horsie_agentcore::EmptyToolbox;
    use serde_json::json;
    use std::str::FromStr;

    async fn toolbox(spaces: &[&str]) -> (MemoryToolbox, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let service = Arc::new(MemoryService::new(MemoryStore::new(pool)));
        for s in spaces {
            if *s != "default" {
                service
                    .create_space(horsie_models::memory::MemorySpaceCreateInput {
                        name: (*s).to_string(),
                        description: None,
                    })
                    .await
                    .unwrap();
            }
        }
        let tb = MemoryToolbox::new(
            Arc::new(EmptyToolbox),
            service,
            spaces.iter().map(|s| (*s).to_string()).collect(),
        );
        (tb, tmp)
    }

    #[tokio::test]
    async fn specs_expose_five_tools_and_pass_through_the_inner_box() {
        let (tb, _t) = toolbox(&["default"]).await;
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        for expected in [
            "memory_load",
            "memory_create",
            "memory_update",
            "memory_delete",
            "memory_list",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 5, "EmptyToolbox contributes nothing");
    }

    #[tokio::test]
    async fn unknown_tool_falls_through_to_the_inner_box() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb.execute("bash", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_then_list_then_load_roundtrip() {
        let (tb, _t) = toolbox(&["default"]).await;
        let created = tb
            .execute(
                "memory_create",
                json!({"name": "alpha", "description": "a fact", "content": "the body"}),
            )
            .await
            .unwrap();
        assert_eq!(created["ref"], "default/alpha");

        let listed = tb.execute("memory_list", json!({})).await.unwrap();
        let items = listed["memories"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["ref"], "default/alpha");
        assert_eq!(items[0]["description"], "a fact");
        assert!(items[0].get("content").is_none(), "list must not ship bodies");

        let loaded = tb
            .execute("memory_load", json!({"refs": ["default/alpha"]}))
            .await
            .unwrap();
        let mems = loaded["memories"].as_array().unwrap();
        assert_eq!(mems[0]["content"], "the body");
    }

    #[tokio::test]
    async fn create_omitting_space_errors_when_several_are_selected() {
        let (tb, _t) = toolbox(&["default", "ops"]).await;
        let err = tb
            .execute(
                "memory_create",
                json!({"name": "alpha", "description": "d", "content": "c"}),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("default") && msg.contains("ops"), "{msg}");
    }

    #[tokio::test]
    async fn writes_outside_the_selected_spaces_are_rejected() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb
            .execute(
                "memory_create",
                json!({"space": "ops", "name": "a", "description": "d", "content": "c"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ops"));
    }

    #[tokio::test]
    async fn duplicate_name_points_at_memory_update() {
        let (tb, _t) = toolbox(&["default"]).await;
        let args = json!({"name": "alpha", "description": "d", "content": "c"});
        tb.execute("memory_create", args.clone()).await.unwrap();
        let err = tb.execute("memory_create", args).await.unwrap_err();
        assert!(err.to_string().contains("memory_update"));
    }

    #[tokio::test]
    async fn load_reports_unknown_refs_without_failing_the_call() {
        let (tb, _t) = toolbox(&["default"]).await;
        tb.execute(
            "memory_create",
            json!({"name": "alpha", "description": "d", "content": "c"}),
        )
        .await
        .unwrap();
        let out = tb
            .execute(
                "memory_load",
                json!({"refs": ["default/alpha", "default/ghost"]}),
            )
            .await
            .unwrap();
        assert_eq!(out["memories"].as_array().unwrap().len(), 1);
        assert_eq!(out["not_found"].as_array().unwrap(), &[json!("default/ghost")]);
    }

    #[tokio::test]
    async fn malformed_ref_is_an_input_error() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb
            .execute("memory_load", json!({"refs": ["alpha"]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("space/name"));
    }

    #[tokio::test]
    async fn update_and_delete_by_ref() {
        let (tb, _t) = toolbox(&["default"]).await;
        tb.execute(
            "memory_create",
            json!({"name": "alpha", "description": "d", "content": "c"}),
        )
        .await
        .unwrap();

        tb.execute(
            "memory_update",
            json!({"ref": "default/alpha", "content": "rewritten"}),
        )
        .await
        .unwrap();
        let loaded = tb
            .execute("memory_load", json!({"refs": ["default/alpha"]}))
            .await
            .unwrap();
        assert_eq!(loaded["memories"][0]["content"], "rewritten");
        assert_eq!(loaded["memories"][0]["description"], "d");

        tb.execute("memory_delete", json!({"ref": "default/alpha"}))
            .await
            .unwrap();
        let listed = tb.execute("memory_list", json!({})).await.unwrap();
        assert!(listed["memories"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_and_delete_reject_refs_outside_the_selected_spaces() {
        let (tb, _t) = toolbox(&["default"]).await;
        for tool in ["memory_update", "memory_delete"] {
            let err = tb
                .execute(tool, json!({"ref": "ops/alpha", "content": "x"}))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("ops"), "{tool}");
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server memory::toolbox`
Expected: FAIL to compile — `MemoryToolbox` does not exist.

- [ ] **Step 3: Implement the tool specs**

Write this above the test module in `server/src/memory/toolbox.rs`:

```rust
//! The agent-facing memory tools. Executes in the server process against
//! SQLite -- the sandboxed runtime is never involved, like `McpToolbox`.
//!
//! Wraps an inner toolbox rather than composing into one, so memory tools sit
//! outside `FilteredToolbox` and a session that sets `allowed_tools` does not
//! silently lose them. The session's selected spaces are the only gate.
//!
//! Specs are static: `CompositeToolbox::execute` calls `specs()` on every box
//! for every tool call, so nothing here may touch the database.

use crate::memory::{MAX_DESCRIPTION_CHARS, MemoryRow, MemoryService, MemoryStore};
use async_trait::async_trait;
use horsie_agentcore::error::ToolCallError;
use horsie_agentcore::{ToolSpec, Toolbox};
use horsie_models::memory::{MemoryCreateInput, MemoryUpdateInput};
use serde_json::{Value, json};
use std::sync::Arc;

const LOAD: &str = "memory_load";
const CREATE: &str = "memory_create";
const UPDATE: &str = "memory_update";
const DELETE: &str = "memory_delete";
const LIST: &str = "memory_list";

pub struct MemoryToolbox {
    inner: Arc<dyn Toolbox>,
    service: Arc<MemoryService>,
    /// The session's selected spaces. Every read and write is confined to
    /// these, so a session cannot reach outside its declared scope.
    spaces: Vec<String>,
}

impl MemoryToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, service: Arc<MemoryService>, spaces: Vec<String>) -> Self {
        Self {
            inner,
            service,
            spaces,
        }
    }

    fn specs_for_memory(&self) -> Vec<ToolSpec> {
        let spaces = self.spaces.join(", ");
        vec![
            ToolSpec {
                name: LOAD.to_string(),
                description: "Read the full text of saved memories. Addresses come from \
                     the memory index in your system prompt, in the form <space>/<name>. \
                     Batch every memory you want in one call."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "refs": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Memory addresses, e.g. [\"default/deploy-order\"]."
                        }
                    },
                    "required": ["refs"]
                }),
            },
            ToolSpec {
                name: CREATE.to_string(),
                description: format!(
                    "Save a new memory. Use this only for something durable and \
                     non-obvious that will matter in a later session -- not for facts the \
                     repository already records. Prefer {UPDATE} when a related memory \
                     already exists. Available spaces: {spaces}."
                ),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "space": {
                            "type": "string",
                            "description": format!(
                                "Which space to save into. Optional when only one space is \
                                 available. Available: {spaces}."
                            )
                        },
                        "name": {
                            "type": "string",
                            "description": "Short slug identifying the memory: lowercase \
                                 letters, digits, '.', '_' and '-'."
                        },
                        "description": {
                            "type": "string",
                            "description": format!(
                                "One line summarising the memory, at most \
                                 {MAX_DESCRIPTION_CHARS} characters. This is all you will \
                                 see in the index later, so make it specific enough to \
                                 decide whether to load the body."
                            )
                        },
                        "content": {
                            "type": "string",
                            "description": "The memory itself, in markdown. Reference \
                                 another memory as [[space/name]]."
                        }
                    },
                    "required": ["name", "description", "content"]
                }),
            },
            ToolSpec {
                name: UPDATE.to_string(),
                description: "Rewrite an existing memory. Supplied fields replace the old \
                     values wholesale; omitted fields are left alone."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Address, <space>/<name>."},
                        "description": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["ref"]
                }),
            },
            ToolSpec {
                name: DELETE.to_string(),
                description: "Delete a memory that is wrong or no longer useful.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Address, <space>/<name>."}
                    },
                    "required": ["ref"]
                }),
            },
            ToolSpec {
                name: LIST.to_string(),
                description: "Re-read the memory index. The index in your system prompt is \
                     a snapshot from the start of the turn, so use this after saving or \
                     deleting something in this same turn."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "space": {
                            "type": "string",
                            "description": format!("Limit to one space. Available: {spaces}.")
                        }
                    }
                }),
            },
        ]
    }
}
```

- [ ] **Step 4: Implement dispatch and the helpers**

Append, still above the test module:

```rust
#[async_trait]
impl Toolbox for MemoryToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(self.specs_for_memory());
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        match name {
            LOAD => self.load(input).await,
            CREATE => self.create(input).await,
            UPDATE => self.update(input).await,
            DELETE => self.delete(input).await,
            LIST => self.list(input).await,
            _ => self.inner.execute(name, input).await,
        }
    }
}

impl MemoryToolbox {
    async fn load(&self, input: Value) -> Result<Value, ToolCallError> {
        let refs = input
            .get("refs")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("'refs' must be an array of memory addresses"))?;
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for r in refs {
            let raw = r
                .as_str()
                .ok_or_else(|| bad("every entry in 'refs' must be a string"))?;
            let (space, name) = self.parse_ref(raw)?;
            match self.service.get_by_ref(&space, &name).await.map_err(exec)? {
                Some(row) => found.push(json!({
                    "ref": format!("{}/{}", row.space, row.name),
                    "description": row.description,
                    "content": row.content,
                    "updated_at": row.updated_at,
                })),
                None => missing.push(json!(raw)),
            }
        }
        Ok(json!({ "memories": found, "not_found": missing }))
    }

    async fn create(&self, input: Value) -> Result<Value, ToolCallError> {
        let space = match input.get("space").and_then(Value::as_str) {
            Some(s) => {
                self.check_space(s)?;
                s.to_string()
            }
            None => match self.spaces.as_slice() {
                [only] => only.clone(),
                _ => {
                    return Err(bad(format!(
                        "'space' is required when several spaces are available: {}",
                        self.spaces.join(", ")
                    )));
                }
            },
        };
        let name = str_arg(&input, "name")?;
        if self
            .service
            .get_by_ref(&space, &name)
            .await
            .map_err(exec)?
            .is_some()
        {
            return Err(bad(format!(
                "memory '{space}/{name}' already exists — use {UPDATE} to change it"
            )));
        }
        let view = self
            .service
            .create_memory(MemoryCreateInput {
                space,
                name,
                description: str_arg(&input, "description")?,
                content: str_arg(&input, "content")?,
            })
            .await
            .map_err(bad)?;
        Ok(json!({
            "ref": format!("{}/{}", view.space, view.name),
            "saved": true,
        }))
    }

    async fn update(&self, input: Value) -> Result<Value, ToolCallError> {
        let row = self.resolve(&input).await?;
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        if description.is_none() && content.is_none() {
            return Err(bad("supply 'description', 'content', or both"));
        }
        self.service
            .update_memory(
                row.id,
                MemoryUpdateInput {
                    description,
                    content,
                },
            )
            .await
            .map_err(bad)?;
        Ok(json!({ "ref": format!("{}/{}", row.space, row.name), "updated": true }))
    }

    async fn delete(&self, input: Value) -> Result<Value, ToolCallError> {
        let row = self.resolve(&input).await?;
        self.service.delete_memory(row.id).await.map_err(exec)?;
        Ok(json!({ "ref": format!("{}/{}", row.space, row.name), "deleted": true }))
    }

    async fn list(&self, input: Value) -> Result<Value, ToolCallError> {
        let spaces = match input.get("space").and_then(Value::as_str) {
            Some(s) => {
                self.check_space(s)?;
                vec![s.to_string()]
            }
            None => self.spaces.clone(),
        };
        let rows = self.service.memories_in(&spaces).await.map_err(exec)?;
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "ref": format!("{}/{}", r.space, r.name),
                    "description": r.description,
                })
            })
            .collect();
        Ok(json!({ "memories": items }))
    }

    /// Split a `space/name` address and confirm the space is one this session
    /// selected.
    fn parse_ref(&self, raw: &str) -> Result<(String, String), ToolCallError> {
        let (space, name) = raw
            .split_once('/')
            .ok_or_else(|| bad(format!("'{raw}' is not a memory address — use space/name")))?;
        if space.is_empty() || name.is_empty() || name.contains('/') {
            return Err(bad(format!(
                "'{raw}' is not a memory address — use space/name"
            )));
        }
        self.check_space(space)?;
        Ok((space.to_string(), name.to_string()))
    }

    fn check_space(&self, space: &str) -> Result<(), ToolCallError> {
        if self.spaces.iter().any(|s| s == space) {
            Ok(())
        } else {
            Err(bad(format!(
                "memory space '{space}' is not available to this session; available: {}",
                self.spaces.join(", ")
            )))
        }
    }

    /// Resolve the `ref` argument of update/delete to an existing row.
    async fn resolve(&self, input: &Value) -> Result<MemoryRow, ToolCallError> {
        let raw = input
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("'ref' is required — a memory address, space/name"))?;
        let (space, name) = self.parse_ref(raw)?;
        self.service
            .get_by_ref(&space, &name)
            .await
            .map_err(exec)?
            .ok_or_else(|| bad(format!("no memory at '{raw}'")))
    }
}

fn str_arg(input: &Value, key: &str) -> Result<String, ToolCallError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad(format!("'{key}' is required and must be a string")))
}

fn bad(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::InvalidInput(msg.into())
}

fn exec(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::ExecutionFailed(msg.into())
}
```

The unused `MemoryStore` import in the module header is only needed by the test helper — if clippy flags it in the non-test build, move it into the test module's `use` list.

- [ ] **Step 5: Uncomment the module declaration and run**

In `server/src/memory/mod.rs`, uncomment `mod toolbox;` and `pub use toolbox::MemoryToolbox;`.

Run: `cargo test -p horsie-server memory::toolbox`
Expected: PASS, 10 tests.

- [ ] **Step 6: Commit**

```bash
git add server/src/memory/
git commit -m "memory: agent toolbox with load/create/update/delete/list"
```

---

### Task 5: Session wiring — selection, toolbox injection, prompt index

**Files:**
- Modify: `models/fluorite/session.fl` (`AgentSettings.memory_spaces`, `SessionDetail.memory_spaces`)
- Modify: `server/src/sessions/spec.rs` (storage `AgentSettings`, `ServerDeps.memory`)
- Modify: `server/src/http/handlers.rs` (`settings_from_wire`, the `detail` mapper)
- Modify: `server/src/sessions/session_actor.rs` (`SessionContextProvider` field + `provide()`)
- Modify: `server/src/sessions/supervisor.rs` (`test_deps`)
- Modify: `server/src/sessions/system_prompt.md` (`## Memories` section)
- Modify: `server/src/bin/horsie-server/main.rs` (`ServerDeps.memory`)
- Test: in-file tests in `server/src/sessions/session_actor.rs`

**Interfaces:**
- Consumes: `MemoryService`, `MemoryToolbox`, `render_index`.
- Produces: `AgentSettings.memory_spaces: Vec<String>` (storage) / `Option<Vec<String>>` (wire); `ServerDeps.memory: Option<Arc<crate::memory::MemoryService>>`; `SessionDetail.memory_spaces: Vec<String>`.

- [ ] **Step 1: Add the wire fields**

In `models/fluorite/session.fl`, add to `AgentSettings` after `mcp_servers`:

```
    /// Memory spaces this session may read and write; absent → none, and the
    /// memory_* tools are not offered.
    memory_spaces: Option<Vec<String>>,
```

and to `SessionDetail` after `mcp_servers`:

```
    /// Selected memory space names (empty when none).
    memory_spaces: Vec<String>,
```

- [ ] **Step 2: Add the storage field and the dependency**

In `server/src/sessions/spec.rs`, add to `AgentSettings` after `mcp_servers`:

```rust
    /// Memory spaces this session may read and write. Empty → the memory tools
    /// are not offered and no index is injected. `#[serde(default)]` so
    /// pre-memory journal rows deserialize.
    #[serde(default)]
    pub memory_spaces: Vec<String>,
```

and to `ServerDeps` after `plugins`:

```rust
    /// Reads and writes the agent's long-term memories, and renders the index
    /// injected into the system prompt; `None` when no memory service is wired
    /// (tests). A session that names spaces with no service configured gets no
    /// memory tools.
    pub memory: Option<Arc<crate::memory::MemoryService>>,
```

Fix the two existing `AgentSettings` literals (`server/src/sessions/session_actor.rs:1111`, `server/src/sessions/supervisor.rs:448`) by adding `memory_spaces: Vec::new(),`, and add `memory: None,` to `test_deps` in `supervisor.rs`.

In `server/src/bin/horsie-server/main.rs`, add `memory: Some(memory.clone()),` to the `ServerDeps` literal — reusing the `memory` binding created in Task 3.

- [ ] **Step 3: Map the wire fields**

In `server/src/http/handlers.rs`, in `settings_from_wire`:

```rust
        memory_spaces: w.memory_spaces.unwrap_or_default(),
```

and in the `detail` mapper (next to the existing `mcp_servers:` line around 204):

```rust
        memory_spaces: rec.spec.agent.memory_spaces.clone(),
```

- [ ] **Step 4: Write the failing `provide()` tests**

Add to the `mod tests` block in `server/src/sessions/session_actor.rs`. These test the two decisions in isolation rather than standing up a whole session:

```rust
#[tokio::test]
async fn memory_index_and_tools_are_absent_when_no_space_is_selected() {
    let (svc, _tmp) = test_memory_service().await;
    let settings = settings_with_spaces(&[]);
    let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

    let (toolbox, index) = build_memory_layer(base, Some(svc), &settings).await.unwrap();
    assert!(index.is_empty());
    assert!(toolbox.specs().is_empty());
}

#[tokio::test]
async fn memory_index_and_tools_appear_when_a_space_is_selected() {
    let (svc, _tmp) = test_memory_service().await;
    svc.create_memory(horsie_models::memory::MemoryCreateInput {
        space: "default".into(),
        name: "alpha".into(),
        description: "a durable fact".into(),
        content: "body".into(),
    })
    .await
    .unwrap();
    let settings = settings_with_spaces(&["default"]);
    let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

    let (toolbox, index) = build_memory_layer(base, Some(svc), &settings).await.unwrap();
    assert!(index.contains("- default/alpha — a durable fact"));
    let names: Vec<String> = toolbox.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"memory_create".to_string()));
}

#[tokio::test]
async fn spaces_selected_with_no_service_wired_degrade_to_nothing() {
    let settings = settings_with_spaces(&["default"]);
    let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

    let (toolbox, index) = build_memory_layer(base, None, &settings).await.unwrap();
    assert!(index.is_empty());
    assert!(toolbox.specs().is_empty());
}
```

Add these helpers to the same test module:

```rust
async fn test_memory_service() -> (Arc<crate::memory::MemoryService>, tempfile::TempDir) {
    use std::str::FromStr;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}/t.db", tmp.path().display());
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    (
        Arc::new(crate::memory::MemoryService::new(
            crate::memory::MemoryStore::new(pool),
        )),
        tmp,
    )
}

fn settings_with_spaces(spaces: &[&str]) -> AgentSettings {
    AgentSettings {
        model: "mock".into(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: 0,
        mcp_servers: Vec::new(),
        memory_spaces: spaces.iter().map(|s| (*s).to_string()).collect(),
    }
}
```

- [ ] **Step 5: Run to verify failure**

Run: `cargo test -p horsie-server sessions::session_actor::tests::memory`
Expected: FAIL to compile — `build_memory_layer` does not exist.

- [ ] **Step 6: Extract `build_memory_layer` and call it from `provide()`**

Add this free function to `server/src/sessions/session_actor.rs`, near `session_run_def`:

```rust
/// Layer the memory tools onto `base` and render the prompt index, for a
/// session's selected spaces. Factored out of `provide()` so both halves of the
/// decision are testable without standing up a session.
///
/// Returns `(base, "")` unchanged when the session selected no spaces, or when
/// it named spaces but no memory service is wired — the tools and the index are
/// offered together or not at all, so the agent is never told about memories it
/// has no way to read.
async fn build_memory_layer(
    base: Arc<dyn Toolbox>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: &AgentSettings,
) -> Result<(Arc<dyn Toolbox>, String), String> {
    let spaces = &settings.memory_spaces;
    if spaces.is_empty() {
        return Ok((base, String::new()));
    }
    let Some(service) = memory else {
        tracing::warn!("session names memory spaces but no memory service is configured; ignoring");
        return Ok((base, String::new()));
    };
    let rows = service.memories_in(spaces).await?;
    let index = crate::memory::render_index(&rows, spaces);
    let toolbox: Arc<dyn Toolbox> = Arc::new(crate::memory::MemoryToolbox::new(
        base,
        service,
        spaces.clone(),
    ));
    Ok((toolbox, index))
}
```

Add the `memory` field to `SessionContextProvider`:

```rust
    memory: Option<Arc<crate::memory::MemoryService>>,
```

and populate it in `ensure_agent` (next to `mcp: self.deps.mcp.clone(),`):

```rust
            memory: self.deps.memory.clone(),
```

Then rewrite the tail of `provide()` — replacing the current `let toolbox = ...` and `let system_prompt = ...` lines:

```rust
        let base: Arc<dyn Toolbox> = DefaultToolboxFactory.for_agent(
            &def,
            self.runtime_client.clone(),
            ws.names(),
            use_plugins,
            mcp,
        );
        let (with_memory, memory_index) =
            build_memory_layer(base, self.memory.clone(), settings).await?;
        let toolbox: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_memory));
        let system_prompt = compose_system_prompt(Some(SESSION_AGENT_PROMPT), &ws, shared.as_ref());
        let system_prompt = match (system_prompt, memory_index.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{memory_index}")),
            (Some(p), true) => Some(p),
            (None, false) => Some(memory_index),
            (None, true) => None,
        };
```

Note `AskUserToolbox` must stay the **outermost** wrapper: `ask_user` is terminal and the agent run looks for it by name via `params.optional_handoff_tool`.

- [ ] **Step 7: Run to verify pass**

Run: `cargo test -p horsie-server sessions::session_actor`
Expected: PASS, including the three new tests.

- [ ] **Step 8: Add the prompt rules**

In `server/src/sessions/system_prompt.md`, insert between the `## Skills` and `## Precedence` sections:

```markdown
## Memories

If a `# Memories` section appears below, you have durable notes from earlier
sessions. Each line gives an address and a one-line summary; load the full text
with the `memory_load` tool before relying on one.

Save a memory when the user asks you to remember something, or when you learn a
fact that is durable, non-obvious, and will matter in a later session. Do not
save what the repository already records — code structure, git history, or
anything in `AGENTS.md` / `CLAUDE.md`. Prefer `memory_update` on an existing
memory over saving a near-duplicate.

Memories are point-in-time observations, not live state. If one makes a claim
about code, verify it against the code before asserting it as fact.
```

- [ ] **Step 9: Regenerate TypeScript types and confirm no drift**

Run: `cd clients/web && bun install && bun run generate-types`
Expected: `src/generated/session/*.ts` gains `memorySpaces` on both `AgentSettings` and `SessionDetail`. Commit the regenerated files.

Run: `cargo test -p horsie-server`
Expected: PASS across the crate.

- [ ] **Step 10: Commit**

```bash
git add models/fluorite/session.fl server/src/sessions/ server/src/http/handlers.rs \
        server/src/bin/horsie-server/main.rs clients/web/src/generated
git commit -m "memory: expose per-session memory spaces to the agent"
```

---

### Task 6: Web management page

**Files:**
- Modify: `clients/web/package.json` (add `memory.fl` to the `generate-types` `-i` list)
- Modify: `clients/web/src/api/types.ts` (re-export `../generated/memory`)
- Modify: `clients/web/src/api/client.ts` (`api.memory` namespace)
- Create: `clients/web/src/hooks/useMemory.ts`
- Create: `clients/web/src/pages/MemoryPage.tsx`
- Modify: `clients/web/src/App.tsx` (route), `clients/web/src/components/Sidebar.tsx` (nav)

**Interfaces:**
- Consumes: the HTTP routes from Task 3 and the generated `memory` types from Task 2.
- Produces: `api.memory.*`, `useMemorySpaces()`, `useMemories(space)`, and the mutation hooks below; a `/memory` route.

- [ ] **Step 1: Add `memory.fl` to codegen and regenerate**

In `clients/web/package.json`, append ` ../../models/fluorite/memory.fl` to the `generate-types` `-i` list, before `-o src/generated`. Do **not** touch `clients/ts/package.json` — that list covers only the session protocol, and `mcp.fl`/`plugins.fl` are already absent from it.

Add to `clients/web/src/api/types.ts`, keeping the list alphabetical:

```ts
export * from "../generated/memory";
```

Run: `cd clients/web && bun run generate-types`
Expected: `src/generated/memory/` appears.

- [ ] **Step 2: Add the `api.memory` namespace**

In `clients/web/src/api/client.ts`, import the new types alongside the existing ones and add this namespace after `plugins`:

```ts
  memory: {
    /** All memory spaces, each with its memory count. */
    listSpaces: (): Promise<MemorySpaceView[]> => request("/memory-spaces"),

    createSpace: (body: MemorySpaceCreateInput): Promise<MemorySpaceView> =>
      request("/memory-spaces", { method: "POST", body: JSON.stringify(body) }),

    /** Rename and/or re-describe; renaming carries the space's memories. */
    updateSpace: (
      name: string,
      body: MemorySpaceUpdateInput,
    ): Promise<MemorySpaceView> =>
      request(`/memory-spaces/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    /** Delete a space and every memory in it. */
    deleteSpace: (name: string): Promise<void> =>
      request(`/memory-spaces/${encodeURIComponent(name)}`, { method: "DELETE" }),

    /** Memories, optionally limited to one space. */
    list: (space?: string): Promise<MemoryView[]> =>
      request(space ? `/memories?space=${encodeURIComponent(space)}` : "/memories"),

    create: (body: MemoryCreateInput): Promise<MemoryView> =>
      request("/memories", { method: "POST", body: JSON.stringify(body) }),

    update: (id: number, body: MemoryUpdateInput): Promise<MemoryView> =>
      request(`/memories/${id}`, { method: "PUT", body: JSON.stringify(body) }),

    remove: (id: number): Promise<void> =>
      request(`/memories/${id}`, { method: "DELETE" }),
  },
```

- [ ] **Step 3: Write `clients/web/src/hooks/useMemory.ts`**

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type {
  MemoryCreateInput,
  MemorySpaceCreateInput,
  MemorySpaceUpdateInput,
  MemoryUpdateInput,
} from "../api/types";

export const memorySpacesKey = ["memory-spaces"] as const;
export const memoriesKey = (space?: string) => ["memories", space ?? "all"] as const;

/** All memory spaces, each with its memory count. */
export function useMemorySpaces() {
  return useQuery({ queryKey: memorySpacesKey, queryFn: () => api.memory.listSpaces() });
}

/** Memories in one space; pass undefined for every space. */
export function useMemories(space?: string) {
  return useQuery({
    queryKey: memoriesKey(space),
    queryFn: () => api.memory.list(space),
  });
}

/** Invalidate both lists — a memory write changes a space's count too. */
function useRefresh() {
  const client = useQueryClient();
  return () => {
    void client.invalidateQueries({ queryKey: memorySpacesKey });
    void client.invalidateQueries({ queryKey: ["memories"] });
  };
}

export function useCreateSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (body: MemorySpaceCreateInput) => api.memory.createSpace(body),
    onSuccess: refresh,
  });
}

export function useUpdateSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: MemorySpaceUpdateInput }) =>
      api.memory.updateSpace(name, body),
    onSuccess: refresh,
  });
}

export function useDeleteSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (name: string) => api.memory.deleteSpace(name),
    onSuccess: refresh,
  });
}

export function useCreateMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (body: MemoryCreateInput) => api.memory.create(body),
    onSuccess: refresh,
  });
}

export function useUpdateMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: ({ id, body }: { id: number; body: MemoryUpdateInput }) =>
      api.memory.update(id, body),
    onSuccess: refresh,
  });
}

export function useDeleteMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (id: number) => api.memory.remove(id),
    onSuccess: refresh,
  });
}
```

- [ ] **Step 4: Write `clients/web/src/pages/MemoryPage.tsx`**

Open `clients/web/src/pages/SkillsPage.tsx` first and copy its page shell, heading, card, button, and input class strings verbatim — the goal is a page that looks like it was always there, not a new visual style. Structure:

```tsx
import { useState } from "react";
import {
  useCreateMemory,
  useCreateSpace,
  useDeleteMemory,
  useDeleteSpace,
  useMemories,
  useMemorySpaces,
  useUpdateMemory,
} from "../hooks/useMemory";

/**
 * Manage the agent's long-term memories: spaces on the left, the selected
 * space's memories on the right. The agent writes here through its memory_*
 * tools; this page is where a human curates what it wrote.
 */
export function MemoryPage() {
  const spaces = useMemorySpaces();
  const [selected, setSelected] = useState<string | null>(null);
  const active = selected ?? spaces.data?.[0]?.name ?? null;
  const memories = useMemories(active ?? undefined);

  const createSpace = useCreateSpace();
  const deleteSpace = useDeleteSpace();
  const createMemory = useCreateMemory();
  const updateMemory = useUpdateMemory();
  const deleteMemory = useDeleteMemory();

  const [newSpace, setNewSpace] = useState("");
  const [editing, setEditing] = useState<number | null>(null);
  const [draft, setDraft] = useState({ name: "", description: "", content: "" });

  // Space list: name, description, memory count, delete button (confirm first —
  // deleting a space takes its memories with it).
  // Memory list: name, description, updated_at, expand-to-edit, delete.
  // Create form: name / description / content, posting into `active`.
  // Render an empty state when there are no spaces, and one when the selected
  // space holds no memories.
  // Surface mutation errors inline (`createMemory.error`), the way SkillsPage
  // surfaces install errors — do not swallow them.
}
```

Fill in the JSX following `SkillsPage.tsx`. Requirements the implementation must meet:

1. Deleting a space asks for confirmation and says how many memories go with it.
2. The create-memory form is disabled when no space is selected.
3. Editing a memory sends only the changed fields (`description` and/or `content`), matching `MemoryUpdateInput`'s optional semantics.
4. `memoryCount` comes from the space view — do not recompute it client-side.
5. Mutation errors render inline; loading states disable the submitting button.

- [ ] **Step 5: Add the route and nav entry**

In `clients/web/src/App.tsx`, alongside the existing `/skills` route:

```tsx
<Route path="memory" element={<MemoryPage />} />
```

In `clients/web/src/components/Sidebar.tsx`, add a "Memory" entry pointing at `/memory`, copying the existing Skills entry's markup and icon treatment.

- [ ] **Step 6: Verify the build**

Run: `cd clients/web && bun run build`
Expected: succeeds, no TypeScript errors, no drift in `src/generated`.

- [ ] **Step 7: Commit**

```bash
git add clients/web/package.json clients/web/src
git commit -m "memory: web page for managing spaces and memories"
```

---

### Task 7: Session memory-space picker

**Files:**
- Modify: `clients/web/src/hooks/useSessionDraft.ts`
- Modify: `clients/web/src/components/SessionConfigBar.tsx`

**Interfaces:**
- Consumes: `useMemorySpaces()` (Task 6), `AgentSettings.memorySpaces` and `SessionDetail.memorySpaces` (Task 5).
- Produces: a `memorySpaces: Set<string>` field on `SessionDraft`, sent as `agent.memorySpaces` in `buildRequest()`.

- [ ] **Step 1: Add the draft field**

In `clients/web/src/hooks/useSessionDraft.ts`, add to the `SessionDraft` interface next to the existing `skills: Set<string>` and `mcp: Set<string>`:

```ts
  /** Memory spaces the session may read and write. */
  memorySpaces: Set<string>;
```

Initialise it to `new Set()` wherever `skills` and `mcp` are initialised, and add to `buildRequest()`, inside the `agent` object next to `mcpServers`:

```ts
      memorySpaces: [...draft.memorySpaces],
```

Note this rides on `agent`, not the top level — `memory_spaces` lives on `AgentSettings`, unlike `plugins` which is a top-level `CreateSessionRequest` field.

- [ ] **Step 2: Add the chip**

In `clients/web/src/components/SessionConfigBar.tsx`, add a memory-spaces multi-select chip in `mode="draft"`, copying the MCP chip at line ~198 (same popover, same multi-select behaviour), sourced from `useMemorySpaces()` and labelled "Memory". For the post-create read-only branch (~288-310), add a `LockedChip` fed by `detail.memorySpaces`, rendered only when the array is non-empty — matching how the plugins and MCP locked chips behave.

- [ ] **Step 3: Verify**

Run: `cd clients/web && bun run build`
Expected: succeeds.

Then manually: `cargo run -p horsie-server --bin horsie-server` in one shell, `cd clients/web && bun run dev` in another. Create a session with the `default` memory space selected, ask the agent to remember something, confirm it appears on `/memory`, then open a new session with the same space and confirm the agent can recall it without being told.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src
git commit -m "memory: pick memory spaces when starting a session"
```

---

### Task 8: Full gate and PR

- [ ] **Step 1: Run the Rust gate**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
```

All four must pass. Fix anything that fails before continuing — do not open the PR on a red gate.

- [ ] **Step 2: Run the TypeScript gate**

```bash
cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && npm run typecheck
git diff --exit-code clients/ts/src/generated
cd ../web && bun run build
git diff --exit-code clients/web/src/generated
```

Both `git diff --exit-code` calls must be clean — a non-empty diff is the ts-drift CI job failing. `clients/ts` should show **no** change at all: `memory.fl` is deliberately not in its codegen list, and the `session.fl` change adds `memorySpaces` to `AgentSettings`/`SessionDetail`, so if `clients/ts` does regenerate, commit that too.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/agent-memory
```

Open the PR against `main`. Body: what it adds (memory spaces, the five tools, the prompt index, the web page), the four design decisions and what each rejected, and the accepted limitations from the spec's final section — no-search, no-dedup, no user scoping, non-deterministic journal replay. Do not hard-wrap the body: GitHub renders newlines as literal breaks, so write one long line per paragraph and per bullet. No AI attribution.

- [ ] **Step 4: Confirm CI is green**

```bash
gh pr checks --watch
```

Expected: Check (fmt/clippy/test), cargo-deny, ts-drift, and both Validate build jobs pass. CI's nightly rustfmt is stricter than local stable `cargo fmt` about import wrapping — if the fmt job fails on imports the local check accepted, apply CI's diff rather than running a local nightly, which reports false repo-wide changes.

---

## Deferred

Not in this plan, recorded so they are visibly out of scope rather than forgotten:

- **Playwright e2e coverage.** The existing `clients/web` e2e harness drives a real server plus mock-LLM; a memory scenario belongs there but needs mock-LLM tool-call scripting. Follow-up.
- **Homelab deploy verification.** Confirm the migration applies cleanly against the live SQLite and that the `default` space seeds, per the deploy runbook.
- **Search, dedup, and background extraction** — see the spec's "Accepted limitations".
