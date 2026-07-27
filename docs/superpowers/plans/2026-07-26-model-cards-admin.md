# Model Cards Admin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an admin-managed, startup-seeded catalog of "model cards" (official model id + context window + max tokens) to horsie-server, consumed by the Settings model form for autocomplete + prefill.

**Architecture:** New `model_cards` SQLite table + `ModelCardStore` (sqlx, same pool as `DbConfigStore`), seeded at server startup from embedded JSON (`INSERT OR IGNORE`). Public prefix-search endpoint `GET /api/model-cards?prefix=`; admin CRUD under `/api/admin/model-cards`. New `/admin` page (sectioned shell) in the React client; Settings model form gains debounced autocomplete on the model-id input. Cards are a prefill catalog only — configured models keep their own copies.

**Tech Stack:** Rust (axum 0.7, sqlx/SQLite, clap), fluorite IDL → Rust + TS codegen, React 19 + react-router 7 + @tanstack/react-query 5, Playwright e2e.

**Spec:** `docs/superpowers/specs/2026-07-26-model-cards-admin-design.md`

## Global Constraints

- Work happens in the worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie-model-cards` on branch `model-cards-admin`.
- Commit messages: short subject only, no body, no AI/tool attribution, no `Co-Authored-By`.
- Clippy runs with `-D warnings`; workspace lints deny `unwrap_used`/`expect_used`/`panic` outside tests. Test modules must open with:
  ```rust
  #[cfg(test)]
  #[allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::wildcard_enum_match_arm
  )]
  mod tests {
  ```
- All HTTP request/response types are fluorite-generated (`models/fluorite/*.fl`). Never hand-write wire structs in Rust or hand-edit `clients/web/src/generated/**` (regenerate with `bun run generate-types`, which needs the `fluorite` CLI on PATH — `cargo install fluorite`).
- JSON on the wire is camelCase (fluorite/serde handles the rename).
- Public prefix search is capped at 50 rows, ordered by `model_id`.
- `model_id` is a card's identity: unique (table PK) and immutable after create (rename = delete + create).
- Seeding is idempotent (`INSERT OR IGNORE`) and MUST NOT overwrite existing rows — admin edits survive restarts.
- The prefill in Settings is a one-time copy into empty fields only; no card→model link is stored.

---

### Task 1: Schema + `ModelCardStore` (migration, fluorite types, store CRUD)

**Files:**
- Create: `server/migrations/0008_model_cards.sql`
- Create: `models/fluorite/model_cards.fl`
- Modify: `models/src/lib.rs` (add `model_cards` module, next to the `settings` module block)
- Create: `server/src/config/model_cards.rs`
- Modify: `server/src/config/mod.rs:7` (add `pub mod model_cards;`)
- Modify: `server/src/config/store.rs:950` (`open_pool` → `pub(crate)`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `horsie_models::model_cards::{ModelCard, ModelCardInput, ModelCardUpdate}` (fluorite-generated; `ModelCard` adds `created_at: String, updated_at: String`).
  - `horsie_server::config::model_cards::{ModelCardStore, ModelCardError, SEARCH_LIMIT}` with:
    - `ModelCardStore::new(pool: sqlx::sqlite::SqlitePool) -> Self`
    - `async fn list(&self) -> Result<Vec<ModelCard>, ModelCardError>`
    - `async fn search_by_prefix(&self, prefix: &str) -> Result<Vec<ModelCard>, ModelCardError>` (capped at `SEARCH_LIMIT = 50`)
    - `async fn get(&self, model_id: &str) -> Result<Option<ModelCard>, ModelCardError>`
    - `async fn insert(&self, input: &ModelCardInput) -> Result<ModelCard, ModelCardError>`
    - `async fn update(&self, model_id: &str, update: &ModelCardUpdate) -> Result<ModelCard, ModelCardError>`
    - `async fn delete(&self, model_id: &str) -> Result<(), ModelCardError>`
    - `async fn seed_if_missing(&self, cards: &[ModelCardInput]) -> Result<usize, ModelCardError>`
  - `ModelCardError::{Invalid(String), Duplicate(String), NotFound(String), Db(String)}`

- [ ] **Step 1: Migration**

Create `server/migrations/0008_model_cards.sql`:

```sql
-- Reference catalog of well-known models ("model cards"): the official
-- provider model id plus its token limits. Managed via /api/admin/model-cards,
-- searched by the Settings model form. Cards are prefill templates only —
-- configured models keep their own copies of these numbers.
CREATE TABLE model_cards (
    model_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    context_window INTEGER,
    max_tokens INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

- [ ] **Step 2: Fluorite schema**

Create `models/fluorite/model_cards.fl`:

```fl
/// Wire types for the model-card catalog: reference records of well-known
/// models (official model id + token limits). Managed via
/// /api/admin/model-cards and searched via /api/model-cards. Cards are
/// prefill templates for the Settings model form — never linked to
/// configured models.
package model_cards;

/// A stored model card.
struct ModelCard {
    /// Official provider model id — the card's identity (e.g. "claude-sonnet-4-6").
    model_id: String,
    /// Display label (e.g. "Claude Sonnet 4.6").
    name: String,
    /// Total context window in tokens.
    context_window: Option<u32>,
    /// Generation cap in tokens.
    max_tokens: Option<u32>,
    created_at: String,
    updated_at: String,
}

/// Create input for a model card.
struct ModelCardInput {
    model_id: String,
    name: String,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
}

/// Update input — `model_id` is immutable (rename = delete + create).
struct ModelCardUpdate {
    name: String,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
}
```

Add the generated module to `models/src/lib.rs` (after the `settings` block):

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod model_cards {
    include!(concat!(env!("OUT_DIR"), "/model_cards/mod.rs"));
}
```

Run `cargo check -p horsie-models` to confirm codegen picks up the new package.

- [ ] **Step 3: Make `open_pool` reusable**

In `server/src/config/store.rs:950`, change `async fn open_pool(` to `pub(crate) async fn open_pool(` so the card store's tests can build a migrated pool without `DbConfigStore`'s deps.

In `server/src/config/mod.rs`, after `mod store;` add:

```rust
pub mod model_cards;
```

- [ ] **Step 4: Write the failing store tests**

Create `server/src/config/model_cards.rs` with the type/method skeletons (bodies `todo!()`-free — leave methods unimplemented by returning `Err(ModelCardError::Db("unimplemented".into()))` so the crate compiles) and this test module:

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

    async fn test_store(dir: &std::path::Path) -> ModelCardStore {
        let pool = crate::config::store::open_pool(&format!("sqlite://{}/t.db", dir.display()))
            .await
            .unwrap();
        ModelCardStore::new(pool)
    }

    fn input(model_id: &str, name: &str, cw: Option<u32>, mt: Option<u32>) -> ModelCardInput {
        ModelCardInput {
            model_id: model_id.into(),
            name: name.into(),
            context_window: cw,
            max_tokens: mt,
        }
    }

    #[tokio::test]
    async fn crud_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;

        let card = store.insert(&input("gpt-4o", "GPT-4o", Some(128_000), Some(16_384))).await.unwrap();
        assert_eq!(card.model_id, "gpt-4o");
        assert!(!card.created_at.is_empty());

        assert_eq!(store.get("gpt-4o").await.unwrap().unwrap().name, "GPT-4o");
        assert!(store.get("nope").await.unwrap().is_none());

        let updated = store
            .update("gpt-4o", &ModelCardUpdate {
                name: "GPT-4o (2024)".into(),
                context_window: Some(128_000),
                max_tokens: Some(16_384),
            })
            .await
            .unwrap();
        assert_eq!(updated.name, "GPT-4o (2024)");

        store.delete("gpt-4o").await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn insert_duplicate_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        store.insert(&input("a", "A", None, None)).await.unwrap();
        assert_eq!(
            store.insert(&input("a", "A2", None, None)).await.unwrap_err(),
            ModelCardError::Duplicate("model card 'a' already exists".into()),
        );
    }

    #[tokio::test]
    async fn validation_rejects_empty_ids_and_zero_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        assert!(matches!(
            store.insert(&input("  ", "A", None, None)).await.unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store.insert(&input("a", " ", None, None)).await.unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store.insert(&input("a", "A", Some(0), None)).await.unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store.insert(&input("a", "A", None, Some(0))).await.unwrap_err(),
            ModelCardError::Invalid(_),
        ));
    }

    #[tokio::test]
    async fn update_and_delete_of_unknown_card_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        assert!(matches!(
            store.update("ghost", &ModelCardUpdate { name: "x".into(), context_window: None, max_tokens: None })
                .await.unwrap_err(),
            ModelCardError::NotFound(_),
        ));
        assert!(matches!(
            store.delete("ghost").await.unwrap_err(),
            ModelCardError::NotFound(_),
        ));
    }

    #[tokio::test]
    async fn prefix_search_orders_limits_and_escapes_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        store.insert(&input("gpt-4o", "GPT-4o", None, None)).await.unwrap();
        store.insert(&input("gpt-4.1", "GPT-4.1", None, None)).await.unwrap();
        store.insert(&input("claude-sonnet-4-6", "Sonnet", None, None)).await.unwrap();
        store.insert(&input("50%_off", "Wildcard", None, None)).await.unwrap();

        let hits = store.search_by_prefix("gpt-4").await.unwrap();
        assert_eq!(hits.iter().map(|c| c.model_id.as_str()).collect::<Vec<_>>(), ["gpt-4.1", "gpt-4o"]);

        assert_eq!(store.search_by_prefix("").await.unwrap().len(), 4);

        // `%` and `_` in the prefix match literally, not as LIKE wildcards.
        let hits = store.search_by_prefix("50%_").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].model_id, "50%_off");
    }

    #[tokio::test]
    async fn seed_if_missing_never_overwrites_existing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        let seeds = vec![input("a", "A", Some(1), None), input("b", "B", None, None)];
        assert_eq!(store.seed_if_missing(&seeds).await.unwrap(), 2);

        store
            .update("a", &ModelCardUpdate { name: "A-edited".into(), context_window: Some(999), max_tokens: None })
            .await
            .unwrap();

        // Reseeding (as happens every boot) inserts nothing and preserves the edit.
        assert_eq!(store.seed_if_missing(&seeds).await.unwrap(), 0);
        let a = store.get("a").await.unwrap().unwrap();
        assert_eq!(a.name, "A-edited");
        assert_eq!(a.context_window, Some(999));
    }
}
```

- [ ] **Step 5: Run tests to verify they fail**

Run: `cargo test -p horsie-server model_cards`
Expected: FAIL (every test errors with "unimplemented").

- [ ] **Step 6: Implement the store**

Replace the skeleton bodies in `server/src/config/model_cards.rs` with:

```rust
//! The model-card catalog: reference records of well-known models (official
//! model id + token limits). Reference data, NOT runtime config — lives
//! outside `DbConfigStore`/`SettingsView`, and no registry rebuild is needed
//! when cards change. Seeded at startup (insert-if-missing), managed via
//! /api/admin/model-cards, searched via /api/model-cards.

use horsie_models::model_cards::{ModelCard, ModelCardInput, ModelCardUpdate};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

/// Cap on rows returned by the public prefix search.
pub const SEARCH_LIMIT: i64 = 50;

#[derive(Debug, PartialEq)]
pub enum ModelCardError {
    /// Rejected input (empty id/name, non-positive limits).
    Invalid(String),
    /// A card with this `model_id` already exists.
    Duplicate(String),
    /// No card with this `model_id`.
    NotFound(String),
    /// Database failure.
    Db(String),
}

pub struct ModelCardStore {
    pool: SqlitePool,
}

fn validate(
    model_id: &str,
    name: &str,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
) -> Result<(), ModelCardError> {
    if model_id.trim().is_empty() {
        return Err(ModelCardError::Invalid("model_id cannot be empty".into()));
    }
    if name.trim().is_empty() {
        return Err(ModelCardError::Invalid("name cannot be empty".into()));
    }
    if context_window == Some(0) || max_tokens == Some(0) {
        return Err(ModelCardError::Invalid(
            "context_window and max_tokens must be positive".into(),
        ));
    }
    Ok(())
}

const COLUMNS: &str = "model_id, name, context_window, max_tokens, created_at, updated_at";

fn row_to_card(r: &sqlx::sqlite::SqliteRow) -> Result<ModelCard, sqlx::Error> {
    let cw: Option<i64> = r.try_get("context_window")?;
    let mt: Option<i64> = r.try_get("max_tokens")?;
    Ok(ModelCard {
        model_id: r.try_get("model_id")?,
        name: r.try_get("name")?,
        context_window: cw.and_then(|v| u32::try_from(v).ok()),
        max_tokens: mt.and_then(|v| u32::try_from(v).ok()),
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

impl ModelCardStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Every card, ordered by `model_id`.
    pub async fn list(&self) -> Result<Vec<ModelCard>, ModelCardError> {
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM model_cards ORDER BY model_id"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
        rows.iter()
            .map(row_to_card)
            .collect::<Result<_, _>>()
            .map_err(|e| ModelCardError::Db(e.to_string()))
    }

    /// Cards whose `model_id` starts with `prefix` (all cards when empty),
    /// ordered by `model_id`, capped at [`SEARCH_LIMIT`]. LIKE wildcards in
    /// the prefix are escaped so they match literally.
    pub async fn search_by_prefix(&self, prefix: &str) -> Result<Vec<ModelCard>, ModelCardError> {
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM model_cards WHERE model_id LIKE ? ESCAPE '\\' \
             ORDER BY model_id LIMIT ?"
        ))
        .bind(format!("{escaped}%"))
        .bind(SEARCH_LIMIT)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        rows.iter()
            .map(row_to_card)
            .collect::<Result<_, _>>()
            .map_err(|e| ModelCardError::Db(e.to_string()))
    }

    pub async fn get(&self, model_id: &str) -> Result<Option<ModelCard>, ModelCardError> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM model_cards WHERE model_id = ?"))
            .bind(model_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
        row.as_ref()
            .map(row_to_card)
            .transpose()
            .map_err(|e| ModelCardError::Db(e.to_string()))
    }

    pub async fn insert(&self, input: &ModelCardInput) -> Result<ModelCard, ModelCardError> {
        validate(&input.model_id, &input.name, input.context_window, input.max_tokens)?;
        if self.get(&input.model_id).await?.is_some() {
            return Err(ModelCardError::Duplicate(format!(
                "model card '{}' already exists",
                input.model_id
            )));
        }
        sqlx::query(
            "INSERT INTO model_cards (model_id, name, context_window, max_tokens) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&input.model_id)
        .bind(&input.name)
        .bind(input.context_window.map(i64::from))
        .bind(input.max_tokens.map(i64::from))
        .execute(&self.pool)
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        self.get(&input.model_id)
            .await?
            .ok_or_else(|| ModelCardError::Db("inserted card vanished".into()))
    }

    /// Update the mutable fields (`model_id` itself is immutable).
    pub async fn update(
        &self,
        model_id: &str,
        update: &ModelCardUpdate,
    ) -> Result<ModelCard, ModelCardError> {
        validate(model_id, &update.name, update.context_window, update.max_tokens)?;
        let res = sqlx::query(
            "UPDATE model_cards SET name = ?, context_window = ?, max_tokens = ?, \
             updated_at = datetime('now') WHERE model_id = ?",
        )
        .bind(&update.name)
        .bind(update.context_window.map(i64::from))
        .bind(update.max_tokens.map(i64::from))
        .bind(model_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        if res.rows_affected() == 0 {
            return Err(ModelCardError::NotFound(format!(
                "no model card '{model_id}'"
            )));
        }
        self.get(model_id)
            .await?
            .ok_or_else(|| ModelCardError::Db("updated card vanished".into()))
    }

    pub async fn delete(&self, model_id: &str) -> Result<(), ModelCardError> {
        let res = sqlx::query("DELETE FROM model_cards WHERE model_id = ?")
            .bind(model_id)
            .execute(&self.pool)
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
        if res.rows_affected() == 0 {
            return Err(ModelCardError::NotFound(format!(
                "no model card '{model_id}'"
            )));
        }
        Ok(())
    }

    /// Insert cards that don't already exist; returns how many were actually
    /// inserted. Existing rows — including admin-edited ones — are never
    /// touched, so reseeding on every boot is safe.
    pub async fn seed_if_missing(
        &self,
        cards: &[ModelCardInput],
    ) -> Result<usize, ModelCardError> {
        let mut inserted = 0usize;
        for c in cards {
            validate(&c.model_id, &c.name, c.context_window, c.max_tokens)?;
            let res = sqlx::query(
                "INSERT OR IGNORE INTO model_cards (model_id, name, context_window, max_tokens) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&c.model_id)
            .bind(&c.name)
            .bind(c.context_window.map(i64::from))
            .bind(c.max_tokens.map(i64::from))
            .execute(&self.pool)
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
            inserted += res.rows_affected() as usize;
        }
        Ok(inserted)
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p horsie-server model_cards`
Expected: PASS (6 tests).

- [ ] **Step 8: Commit**

```bash
git add server/migrations/0008_model_cards.sql models/fluorite/model_cards.fl models/src/lib.rs server/src/config/
git commit -m "server: model cards table + store"
```

---

### Task 2: Startup seeding (bundled defaults + optional operator seed file)

**Files:**
- Create: `server/src/config/model_cards_seed.json`
- Modify: `server/src/config/model_cards.rs` (seed loaders + tests)
- Modify: `server/src/bin/horsie-server/main.rs:40-52` (CLI flag), `:109-117` (seed after store open)

**Interfaces:**
- Consumes: `ModelCardStore::seed_if_missing`, `ModelCardInput` (Task 1).
- Produces:
  - `horsie_server::config::model_cards::bundled_seed() -> Result<Vec<ModelCardInput>, String>`
  - `horsie_server::config::model_cards::load_seed_file(path: &Path) -> Result<Vec<ModelCardInput>, String>`
  - `horsie-server --model-cards-seed <path>` flag, `HORSIE_MODEL_CARDS_SEED` env fallback (Task 3's `main.rs` AppState wiring assumes a `model_cards: Arc<ModelCardStore>` exists in `run()` scope).

- [ ] **Step 1: Bundled seed JSON**

Create `server/src/config/model_cards_seed.json` (camelCase — the fluorite serde rename applies):

```json
[
  { "modelId": "claude-haiku-4-5", "name": "Claude Haiku 4.5", "contextWindow": 200000, "maxTokens": 8192 },
  { "modelId": "claude-opus-4-1", "name": "Claude Opus 4.1", "contextWindow": 200000, "maxTokens": 32768 },
  { "modelId": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6", "contextWindow": 200000, "maxTokens": 16384 },
  { "modelId": "deepseek-chat", "name": "DeepSeek Chat", "contextWindow": 128000, "maxTokens": 8192 },
  { "modelId": "gpt-4.1", "name": "GPT-4.1", "contextWindow": 1000000, "maxTokens": 32768 },
  { "modelId": "gpt-4o", "name": "GPT-4o", "contextWindow": 128000, "maxTokens": 16384 },
  { "modelId": "o3", "name": "o3", "contextWindow": 200000, "maxTokens": 32768 }
]
```

(The e2e test in Task 7 asserts against `claude-sonnet-4-6` / 200000 / 16384 — keep this entry exactly.)

- [ ] **Step 2: Write the failing seed tests**

Add to the `tests` module in `server/src/config/model_cards.rs`:

```rust
    #[test]
    fn bundled_seed_parses_and_is_valid() {
        let cards = bundled_seed().unwrap();
        assert!(cards.len() >= 7);
        assert!(cards.iter().any(|c| c.model_id == "claude-sonnet-4-6"));
    }

    #[tokio::test]
    async fn operator_seed_file_merges_with_same_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        store.seed_if_missing(&bundled_seed().unwrap()).await.unwrap();

        let path = dir.path().join("extra.json");
        std::fs::write(
            &path,
            r#"[{"modelId":"my-local-model","name":"Local","contextWindow":32000,"maxTokens":2048}]"#,
        )
        .unwrap();
        let extra = load_seed_file(&path).unwrap();
        assert_eq!(store.seed_if_missing(&extra).await.unwrap(), 1);
        assert!(store.get("my-local-model").await.unwrap().is_some());
        // Bundled cards are still there and untouched.
        assert!(store.get("claude-sonnet-4-6").await.unwrap().is_some());
    }

    #[test]
    fn invalid_operator_seed_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_seed_file(&path).is_err());
        assert!(load_seed_file(&dir.path().join("missing.json")).is_err());
        let invalid = dir.path().join("invalid.json");
        std::fs::write(&invalid, r#"[{"modelId":"","name":"x"}]"#).unwrap();
        assert!(load_seed_file(&invalid).is_err());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p horsie-server model_cards`
Expected: FAIL — `bundled_seed` / `load_seed_file` unresolved (compile error).

- [ ] **Step 4: Implement the seed loaders**

Add to `server/src/config/model_cards.rs` (after the `ModelCardStore` impl):

```rust
/// The compiled-in default catalog, seeded at every startup (insert-if-missing).
const BUNDLED_SEED_JSON: &str = include_str!("model_cards_seed.json");

/// Parse the bundled seed. An error here is a build-time bug — the JSON is
/// compiled into the binary.
pub fn bundled_seed() -> Result<Vec<ModelCardInput>, String> {
    parse_seed(BUNDLED_SEED_JSON).map_err(|e| format!("bundled model-cards seed is invalid: {e}"))
}

/// Read + parse an operator-supplied seed file (`--model-cards-seed`).
pub fn load_seed_file(path: &std::path::Path) -> Result<Vec<ModelCardInput>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read model-cards seed {}: {e}", path.display()))?;
    parse_seed(&text).map_err(|e| format!("parse model-cards seed {}: {e}", path.display()))
}

fn parse_seed(json: &str) -> Result<Vec<ModelCardInput>, String> {
    let cards: Vec<ModelCardInput> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    for c in &cards {
        validate(&c.model_id, &c.name, c.context_window, c.max_tokens).map_err(|e| match e {
            ModelCardError::Invalid(m) => m,
            other => format!("{other:?}"),
        })?;
    }
    Ok(cards)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie-server model_cards`
Expected: PASS (9 tests).

- [ ] **Step 6: Wire seeding into `horsie-server` startup**

In `server/src/bin/horsie-server/main.rs`:

Add the CLI flag to `struct Cli` (after `web`):

```rust
    /// JSON file of extra model cards to seed at startup (insert-if-missing;
    /// bundled defaults are always seeded). Also read from
    /// $HORSIE_MODEL_CARDS_SEED.
    #[arg(long)]
    model_cards_seed: Option<PathBuf>,
```

Update the import: `use horsie_server::config::{DbConfigStore, StoreDeps};` → add `model_cards`:

```rust
use horsie_server::config::{DbConfigStore, StoreDeps, model_cards};
```

In `run()`, immediately after the `DbConfigStore::open(...)` block (`:109-117`), add:

```rust
    // Seed the model-card catalog: bundled defaults plus an optional operator
    // file. Seed-file parse/read errors are fatal (operator input should fail
    // loud); DB errors only warn — the admin API stays usable to fix state.
    // Insert-if-missing semantics mean admin edits survive every restart.
    let model_cards = std::sync::Arc::new(model_cards::ModelCardStore::new(opened.pool.clone()));
    let seed_path = cli
        .model_cards_seed
        .clone()
        .or_else(|| std::env::var_os("HORSIE_MODEL_CARDS_SEED").map(PathBuf::from));
    let seeding = (|| -> Result<Vec<horsie_models::model_cards::ModelCardInput>, BootError> {
        let mut seeds = model_cards::bundled_seed().map_err(BootError::Config)?;
        if let Some(path) = seed_path {
            seeds.extend(model_cards::load_seed_file(&path).map_err(BootError::Config)?);
        }
        Ok(seeds)
    })();
    match seeding {
        Ok(seeds) => {
            if let Err(e) = model_cards.seed_if_missing(&seeds).await {
                eprintln!("warning: seeding model cards failed: {e:?}");
            }
        }
        Err(e) => return Err(e),
    }
```

(`horsie_models` is already an extern crate of the server binary — check `main.rs` imports: it uses `horsie_models::capabilities` and `horsie_models::settings`, so no Cargo.toml change is needed.)

- [ ] **Step 7: Verify the server binary compiles and gates pass for the crate**

Run: `cargo check -p horsie-server && cargo clippy -p horsie-server --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add server/src/config/model_cards_seed.json server/src/config/model_cards.rs server/src/bin/horsie-server/main.rs
git commit -m "server: seed model cards at startup"
```

---

### Task 3: HTTP API (public prefix search + admin CRUD)

**Files:**
- Create: `server/src/http/model_cards.rs` (public handler)
- Create: `server/src/http/admin.rs` (admin handlers)
- Modify: `server/src/http/mod.rs` (module decls, `AppState.model_cards`, routes, `test_state`, route tests)
- Modify: `server/src/bin/horsie-server/main.rs` (`AppState` construction gains `model_cards`)

**Interfaces:**
- Consumes: `ModelCardStore`, `ModelCardError`, `ModelCard{,Input,Update}` (Tasks 1–2); `Api` error constructors (`not_found`/`conflict`/`unprocessable`/`internal`).
- Produces:
  - `GET /api/model-cards?prefix=<s>` → `200 [ModelCard]`
  - `GET /api/admin/model-cards` → `200 [ModelCard]`
  - `POST /api/admin/model-cards` body `ModelCardInput` → `201 ModelCard` | 409 | 422
  - `PUT /api/admin/model-cards/:model_id` body `ModelCardUpdate` → `200 ModelCard` | 404 | 422
  - `DELETE /api/admin/model-cards/:model_id` → `204` | 404
  - `AppState.model_cards: Arc<ModelCardStore>` (web tasks don't touch this; e2e hits the routes).

- [ ] **Step 1: Write the failing route tests**

Add to the `tests` module in `server/src/http/mod.rs` (inside `mod tests`, after the existing tests):

```rust
    #[tokio::test]
    async fn model_cards_public_prefix_search() {
        use horsie_models::model_cards::{ModelCard, ModelCardInput};
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let store = state.model_cards.clone();
        let app = app(state);

        let input = |id: &str| ModelCardInput {
            model_id: id.into(),
            name: id.into(),
            context_window: Some(1000),
            max_tokens: None,
        };
        store.seed_if_missing(&[input("gpt-4o"), input("gpt-4.1"), input("claude-sonnet-4-6")])
            .await
            .unwrap();

        let res = app.clone().oneshot(get("/api/model-cards")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let all: Vec<ModelCard> = read_json(res).await;
        assert_eq!(all.len(), 3);

        let res = app
            .oneshot(get("/api/model-cards?prefix=gpt-4"))
            .await
            .unwrap();
        let hits: Vec<ModelCard> = read_json(res).await;
        assert_eq!(
            hits.iter().map(|c| c.model_id.as_str()).collect::<Vec<_>>(),
            ["gpt-4.1", "gpt-4o"]
        );
    }

    #[tokio::test]
    async fn admin_model_cards_crud_over_http() {
        use horsie_models::model_cards::ModelCard;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);

        // Empty catalog (test_state does not seed).
        let res = app.clone().oneshot(get("/api/admin/model-cards")).await.unwrap();
        let list: Vec<ModelCard> = read_json(res).await;
        assert!(list.is_empty());

        // Create.
        let body = serde_json::json!({"modelId": "m1", "name": "Model One", "contextWindow": 8192});
        let res = app.clone().oneshot(post_json("/api/admin/model-cards", &body)).await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let card: ModelCard = read_json(res).await;
        assert_eq!(card.model_id, "m1");
        assert_eq!(card.max_tokens, None);

        // Duplicate → 409.
        let res = app.clone().oneshot(post_json("/api/admin/model-cards", &body)).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Invalid → 422.
        let bad = serde_json::json!({"modelId": "", "name": "x"});
        let res = app.clone().oneshot(post_json("/api/admin/model-cards", &bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Update.
        let upd = serde_json::json!({"name": "Model 1 Renamed", "maxTokens": 2048});
        let res = app.clone().oneshot(put_json("/api/admin/model-cards/m1", &upd)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let card: ModelCard = read_json(res).await;
        assert_eq!(card.name, "Model 1 Renamed");
        assert_eq!(card.max_tokens, Some(2048));

        // Update of unknown → 404.
        let res = app.clone().oneshot(put_json("/api/admin/model-cards/ghost", &upd)).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Delete → 204; second delete → 404.
        let res = app.clone().oneshot(delete("/api/admin/model-cards/m1")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app.oneshot(delete("/api/admin/model-cards/m1")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server http::tests`
Expected: FAIL to compile — `state.model_cards` doesn't exist.

- [ ] **Step 3: Public handler**

Create `server/src/http/model_cards.rs`:

```rust
//! Public model-card read API: prefix search consumed by the Settings model
//! form's model-id autocomplete. Mutations live under `/api/admin`.

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Query, State};
use horsie_models::model_cards::ModelCard;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    prefix: Option<String>,
}

/// `GET /api/model-cards?prefix=` — cards whose `model_id` starts with
/// `prefix` (all cards when omitted), ordered by `model_id`, capped.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<ModelCard>>, Api> {
    state
        .model_cards
        .search_by_prefix(q.prefix.as_deref().unwrap_or(""))
        .await
        .map(Json)
        .map_err(super::admin::map_card_err)
}
```

- [ ] **Step 4: Admin handlers**

Create `server/src/http/admin.rs`:

```rust
//! Admin API: operator-facing management surfaces. v1 is the model-card
//! catalog; future admin settings add handlers here and routes under
//! `/api/admin`. Unauthenticated like the rest of `/api/*` (single-user,
//! localhost-bound deployment).

use crate::config::model_cards::ModelCardError;
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::model_cards::{ModelCard, ModelCardInput, ModelCardUpdate};

/// Map a store error onto the HTTP envelope: 422 invalid input, 409
/// duplicate id, 404 unknown id, 500 anything else.
pub(crate) fn map_card_err(e: ModelCardError) -> Api {
    match e {
        ModelCardError::Invalid(m) => Api::unprocessable(m),
        ModelCardError::Duplicate(m) => Api::conflict("duplicate_model_id", m),
        ModelCardError::NotFound(m) => Api::not_found(m),
        ModelCardError::Db(m) => Api::internal(m),
    }
}

/// `GET /api/admin/model-cards` — the full catalog (kept separate from the
/// public search so admin-only fields can be added later without touching
/// the public contract).
pub async fn list_cards(State(state): State<AppState>) -> Result<Json<Vec<ModelCard>>, Api> {
    state.model_cards.list().await.map(Json).map_err(map_card_err)
}

/// `POST /api/admin/model-cards` — create a card; 409 on duplicate `model_id`.
pub async fn create_card(
    State(state): State<AppState>,
    Json(input): Json<ModelCardInput>,
) -> Result<(StatusCode, Json<ModelCard>), Api> {
    state
        .model_cards
        .insert(&input)
        .await
        .map(|card| (StatusCode::CREATED, Json(card)))
        .map_err(map_card_err)
}

/// `PUT /api/admin/model-cards/:model_id` — update name/limits. `model_id`
/// itself is immutable (rename = delete + create).
pub async fn update_card(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(update): Json<ModelCardUpdate>,
) -> Result<Json<ModelCard>, Api> {
    state
        .model_cards
        .update(&model_id, &update)
        .await
        .map(Json)
        .map_err(map_card_err)
}

/// `DELETE /api/admin/model-cards/:model_id` — 204 on success, 404 when absent.
pub async fn delete_card(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .model_cards
        .delete(&model_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(map_card_err)
}
```

- [ ] **Step 5: Wire modules, AppState, routes**

In `server/src/http/mod.rs`:

Module declarations (`:4-11`) — add:

```rust
mod admin;
```

and after `mod mcp;` add:

```rust
mod model_cards;
```

`AppState` — add after the `config_store` field:

```rust
    /// The model-card catalog (reference data, not runtime config): public
    /// prefix search + admin CRUD. Shares the settings-DB pool.
    pub model_cards: Arc<crate::config::model_cards::ModelCardStore>,
```

Routes — add after the `/api/config/vendors/:name/test` route:

```rust
        .route("/api/model-cards", get(model_cards::list))
        .route(
            "/api/admin/model-cards",
            get(admin::list_cards).post(admin::create_card),
        )
        .route(
            "/api/admin/model-cards/:model_id",
            put(admin::update_card).delete(admin::delete_card),
        )
```

`test_state` helper — after the `mcp` construction add:

```rust
        let model_cards = Arc::new(crate::config::model_cards::ModelCardStore::new(
            opened.pool.clone(),
        ));
```

and add `model_cards,` to the returned `AppState { ... }` literal.

In `server/src/bin/horsie-server/main.rs`, add `model_cards,` to the `AppState { ... }` literal (the variable from Task 2 Step 6 is already in scope).

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p horsie-server http::tests`
Expected: PASS — all existing tests plus the 2 new ones.

- [ ] **Step 7: Commit**

```bash
git add server/src/http/ server/src/bin/horsie-server/main.rs
git commit -m "server: model cards HTTP API (public search + admin CRUD)"
```

---

### Task 4: Web plumbing (TS types, API client, hooks)

**Files:**
- Modify: `clients/web/package.json:14` (add `model_cards.fl` to `generate-types`)
- Modify: `clients/web/src/api/types.ts` (re-export)
- Create: `clients/web/src/generated/model_cards/**` (generated — do not hand-edit)
- Modify: `clients/web/src/api/client.ts` (imports + `modelCards` + `admin` groups)
- Create: `clients/web/src/hooks/useModelCards.ts`

**Interfaces:**
- Consumes: the HTTP routes from Task 3; TS types `ModelCard`, `ModelCardInput`, `ModelCardUpdate` (camelCase fields: `modelId`, `name`, `contextWindow`, `maxTokens`, `createdAt`, `updatedAt`).
- Produces:
  - `api.modelCards.search(prefix?: string): Promise<ModelCard[]>`
  - `api.admin.modelCards.{list, create, update, remove}`
  - Hooks `useModelCardSearch(prefix, enabled?)`, `useAdminModelCards()`, `useCreateModelCard()`, `useUpdateModelCard()`, `useDeleteModelCard()` and `modelCardsKey`.

- [ ] **Step 1: Regenerate TS types**

In `clients/web/package.json`, add `../../models/fluorite/model_cards.fl` to the end of the `-i` list in the `generate-types` script.

Run: `cd clients/web && bun run generate-types`
Expected: `src/generated/model_cards/` appears with `modelCard.ts`, `modelCardInput.ts`, `modelCardUpdate.ts`, `index.ts`.

Add to `clients/web/src/api/types.ts`:

```ts
export * from "../generated/model_cards";
```

- [ ] **Step 2: API client**

In `clients/web/src/api/client.ts`, add `ModelCard`, `ModelCardInput`, `ModelCardUpdate` to the type imports from `./types`, then add to the `api` object (after the `config` group):

```ts
  modelCards: {
    /** Public: cards whose modelId starts with `prefix` (all when ""). */
    search: (prefix = ""): Promise<ModelCard[]> =>
      request(
        `/model-cards${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ""}`,
      ),
  },

  admin: {
    modelCards: {
      list: (): Promise<ModelCard[]> => request("/admin/model-cards"),

      create: (body: ModelCardInput): Promise<ModelCard> =>
        request("/admin/model-cards", {
          method: "POST",
          body: JSON.stringify(body),
        }),

      /** Update name/limits; `modelId` is immutable. */
      update: (modelId: string, body: ModelCardUpdate): Promise<ModelCard> =>
        request(`/admin/model-cards/${encodeURIComponent(modelId)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        }),

      remove: (modelId: string): Promise<void> =>
        request(`/admin/model-cards/${encodeURIComponent(modelId)}`, {
          method: "DELETE",
        }),
    },
  },
```

- [ ] **Step 3: Hooks**

Create `clients/web/src/hooks/useModelCards.ts`:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { ModelCardInput, ModelCardUpdate } from "../api/types";

export const modelCardsKey = ["model-cards"] as const;

/** Public prefix search — backs the Settings model-id autocomplete. */
export function useModelCardSearch(prefix: string, enabled = true) {
  return useQuery({
    queryKey: [...modelCardsKey, "search", prefix],
    queryFn: () => api.modelCards.search(prefix),
    enabled,
  });
}

/** The full catalog, for the admin page. */
export function useAdminModelCards() {
  return useQuery({
    queryKey: [...modelCardsKey, "admin"],
    queryFn: () => api.admin.modelCards.list(),
  });
}

export function useCreateModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: ModelCardInput) => api.admin.modelCards.create(body),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}

export function useUpdateModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({
      modelId,
      body,
    }: {
      modelId: string;
      body: ModelCardUpdate;
    }) => api.admin.modelCards.update(modelId, body),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}

export function useDeleteModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (modelId: string) => api.admin.modelCards.remove(modelId),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}
```

- [ ] **Step 4: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add clients/web/package.json clients/web/src/api/ clients/web/src/generated/model_cards clients/web/src/hooks/useModelCards.ts
git commit -m "web: model cards api client + hooks"
```

---

### Task 5: `/admin` page with the model-cards section

**Files:**
- Create: `clients/web/src/pages/AdminPage.tsx`
- Modify: `clients/web/src/App.tsx` (route)
- Modify: `clients/web/src/components/Sidebar.tsx` (nav link)

**Interfaces:**
- Consumes: Task 4 hooks; lucide `ShieldCheck` icon; `ApiRequestError` from `api/client`.
- Produces: `/admin` route rendering the catalog with inline add/edit/delete rows (pattern follows `McpServerRow` in `SettingsPage.tsx` — per-resource save, `confirm()` on delete, like `SkillsPage`). Testids: `add-model-card`, `model-card-row-new`, `model-card-row-<modelId>`, `model-card-save`, `model-card-remove`.

- [ ] **Step 1: The page**

Create `clients/web/src/pages/AdminPage.tsx`:

```tsx
import { Loader2, Plus, Save, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { ApiRequestError } from "../api/client";
import type { ModelCard } from "../api/types";
import {
  useAdminModelCards,
  useCreateModelCard,
  useDeleteModelCard,
  useUpdateModelCard,
} from "../hooks/useModelCards";

/** Admin: operator-facing management surfaces. Model cards is the first
 * section; future admin settings add another `<section>` below. */
export function AdminPage() {
  return (
    <div className="flex h-full flex-col">
      <header className="flex items-center border-b px-6 py-3">
        <h1 className="text-sm font-semibold text-text">Admin</h1>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          <ModelCardsSection />
        </div>
      </div>
    </div>
  );
}

function ModelCardsSection() {
  const { data: cards, isLoading, isError } = useAdminModelCards();
  const [adding, setAdding] = useState(false);
  return (
    <section className="card p-4">
      <div className="mb-3 flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-text">Model cards</h2>
          <p className="mt-0.5 text-xs text-faint">
            Well-known models and their token limits. The Settings model form
            autocompletes model ids from these and prefills empty limit
            fields — editing a card never changes already-configured models.
          </p>
        </div>
        <button
          className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs"
          onClick={() => setAdding(true)}
          data-testid="add-model-card"
        >
          <Plus size={14} /> Add card
        </button>
      </div>
      <div className="space-y-2.5">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && (
          <p className="text-sm text-error">Couldn’t load model cards.</p>
        )}
        {cards?.length === 0 && !adding && (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
            No model cards.
          </p>
        )}
        {adding && <ModelCardRow onDone={() => setAdding(false)} />}
        {cards?.map((c) => <ModelCardRow key={c.modelId} card={c} />)}
      </div>
    </section>
  );
}

function RowLabel({ children }: { children: ReactNode }) {
  return (
    <span className="mb-1 block text-[11px] font-semibold text-muted">
      {children}
    </span>
  );
}

/** One card row for both a new (unsaved) and an existing card. Save creates
 * or updates immediately; Remove deletes (or drops the new draft). The model
 * id is the id of record, so it is fixed once saved. */
function ModelCardRow({
  card,
  onDone,
}: {
  card?: ModelCard;
  onDone?: () => void;
}) {
  const create = useCreateModelCard();
  const update = useUpdateModelCard();
  const remove = useDeleteModelCard();
  const isNew = !card;

  const [modelId, setModelId] = useState(card?.modelId ?? "");
  const [name, setName] = useState(card?.name ?? "");
  const [contextWindow, setContextWindow] = useState(
    card?.contextWindow != null ? String(card.contextWindow) : "",
  );
  const [maxTokens, setMaxTokens] = useState(
    card?.maxTokens != null ? String(card.maxTokens) : "",
  );
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const touch = () => setDirty(true);

  const parseNum = (s: string): number | undefined =>
    s.trim() === "" ? undefined : Number(s);

  const save = async () => {
    setError(null);
    if (isNew && !modelId.trim()) return setError("Model id is required.");
    if (!name.trim()) return setError("Name is required.");
    for (const [label, v] of [
      ["Context window", contextWindow],
      ["Max tokens", maxTokens],
    ] as const) {
      if (v.trim() !== "" && (!Number.isInteger(Number(v)) || Number(v) <= 0))
        return setError(`${label} must be a positive whole number.`);
    }
    try {
      if (isNew) {
        await create.mutateAsync({
          modelId: modelId.trim(),
          name: name.trim(),
          contextWindow: parseNum(contextWindow),
          maxTokens: parseNum(maxTokens),
        });
        onDone?.();
      } else {
        await update.mutateAsync({
          modelId: card.modelId,
          body: {
            name: name.trim(),
            contextWindow: parseNum(contextWindow),
            maxTokens: parseNum(maxTokens),
          },
        });
        setDirty(false);
      }
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Save failed.");
    }
  };

  const onRemove = async () => {
    setError(null);
    if (isNew) return onDone?.();
    if (
      !confirm(
        `Delete model card "${card.modelId}"? Models already configured keep their current values.`,
      )
    )
      return;
    try {
      await remove.mutateAsync(card.modelId);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Delete failed.");
    }
  };

  const pending = create.isPending || update.isPending || remove.isPending;

  return (
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
      data-testid={isNew ? "model-card-row-new" : `model-card-row-${card.modelId}`}
    >
      <div className="grid grid-cols-2 gap-3">
        <label className="block">
          <RowLabel>Model id</RowLabel>
          <input
            className="input font-mono"
            value={modelId}
            onChange={(e) => {
              setModelId(e.target.value);
              touch();
            }}
            placeholder="claude-sonnet-4-6"
            disabled={!isNew}
          />
        </label>
        <label className="block">
          <RowLabel>Name</RowLabel>
          <input
            className="input"
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              touch();
            }}
            placeholder="Claude Sonnet 4.6"
          />
        </label>
        <label className="block">
          <RowLabel>Context window (optional)</RowLabel>
          <input
            className="input font-mono"
            value={contextWindow}
            onChange={(e) => {
              setContextWindow(e.target.value);
              touch();
            }}
            placeholder="200000"
          />
        </label>
        <label className="block">
          <RowLabel>Max tokens (optional)</RowLabel>
          <input
            className="input font-mono"
            value={maxTokens}
            onChange={(e) => {
              setMaxTokens(e.target.value);
              touch();
            }}
            placeholder="16384"
          />
        </label>
      </div>

      {error && (
        <div className="mt-3 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
          {error}
        </div>
      )}

      <div className="mt-3 flex items-center justify-end gap-2">
        <button
          className="btn-icon text-faint hover:text-error"
          onClick={onRemove}
          aria-label={isNew ? "Discard new card" : "Delete card"}
          data-testid="model-card-remove"
          disabled={pending}
        >
          <Trash2 size={15} />
        </button>
        <button
          className="btn-primary !px-2.5 !py-1.5 text-xs"
          onClick={save}
          disabled={(!isNew && !dirty) || pending}
          data-testid="model-card-save"
        >
          {pending ? (
            <Loader2 size={14} className="animate-spin" />
          ) : (
            <Save size={14} />
          )}
          {isNew ? "Add card" : "Save"}
        </button>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: Route + nav**

In `clients/web/src/App.tsx`: import `AdminPage` and add inside the `SessionsLayout` route (after `skills`):

```tsx
            <Route path="admin" element={<AdminPage />} />
```

In `clients/web/src/components/Sidebar.tsx`: add `ShieldCheck` to the lucide import, and after the Skills `NavLink` add:

```tsx
          <NavLink
            to="/admin"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-1.5 rounded-[var(--radius)] px-2 py-1.5 text-xs font-medium transition-colors",
                isActive
                  ? "bg-surface-3 text-text"
                  : "text-muted hover:bg-surface-2 hover:text-text",
              )
            }
          >
            <ShieldCheck size={14} />
            Admin
          </NavLink>
```

- [ ] **Step 3: Typecheck + build**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/pages/AdminPage.tsx clients/web/src/App.tsx clients/web/src/components/Sidebar.tsx
git commit -m "web: /admin page with model cards section"
```

---

### Task 6: Settings model-id autocomplete + prefill

**Files:**
- Modify: `clients/web/src/pages/SettingsPage.tsx` (new `ModelIdField`; use it in `ModelRow` at `:843-848`)

**Interfaces:**
- Consumes: `useModelCardSearch` (Task 4); `ModelCard` type.
- Produces: model-id input with debounced suggestions; selecting one sets `modelId` and fills `maxTokens`/`contextWindow` only where empty. Testids: `model-id-input`, `model-card-suggestions`, `model-card-suggestion-<modelId>`.

- [ ] **Step 1: The autocomplete field**

In `clients/web/src/pages/SettingsPage.tsx`:

Add `ModelCard` to the type imports from `../api/types`, and add the hook import:

```ts
import { useModelCardSearch } from "../hooks/useModelCards";
```

Add this component (place it just above `function ModelRow`):

```tsx
/** The model-id input with card-backed autocomplete: typing queries the
 * catalog by prefix; picking a suggestion sets the id and prefills the
 * limit fields that are still empty. Prefill is a one-time copy — every
 * field stays editable, and no link to the card is kept. */
function ModelIdField({
  draft,
  set,
}: {
  draft: ModelDraft;
  set: (patch: Partial<ModelDraft>) => void;
}) {
  const [focused, setFocused] = useState(false);
  const [debounced, setDebounced] = useState(draft.modelId);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(draft.modelId), 200);
    return () => clearTimeout(t);
  }, [draft.modelId]);
  const query = debounced.trim();
  const { data: suggestions } = useModelCardSearch(query, focused && query.length > 0);
  const show = focused && query.length > 0 && (suggestions?.length ?? 0) > 0;

  const pick = (card: ModelCard) => {
    set({
      modelId: card.modelId,
      maxTokens:
        draft.maxTokens === "" && card.maxTokens != null
          ? String(card.maxTokens)
          : draft.maxTokens,
      contextWindow:
        draft.contextWindow === "" && card.contextWindow != null
          ? String(card.contextWindow)
          : draft.contextWindow,
    });
    setFocused(false);
  };

  return (
    <label className="relative block">
      <RowLabel>Model id</RowLabel>
      <input
        className="input font-mono"
        value={draft.modelId}
        onChange={(e) => set({ modelId: e.target.value })}
        onFocus={() => setFocused(true)}
        // Delay so an onMouseDown on a suggestion fires before the list hides.
        onBlur={() => setTimeout(() => setFocused(false), 150)}
        placeholder="claude-sonnet-4-6"
        data-testid="model-id-input"
      />
      {show && (
        <ul
          className="absolute z-10 mt-1 max-h-48 w-full overflow-y-auto rounded-[var(--radius)] border shadow-lg"
          style={{ background: "var(--surface)" }}
          data-testid="model-card-suggestions"
        >
          {suggestions!.map((c) => (
            <li key={c.modelId}>
              <button
                type="button"
                className="flex w-full items-baseline justify-between gap-2 px-2.5 py-1.5 text-left text-xs hover:bg-surface-2"
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(c);
                }}
                data-testid={`model-card-suggestion-${c.modelId}`}
              >
                <span className="font-mono text-text">{c.modelId}</span>
                <span className="truncate text-faint">
                  {c.name}
                  {c.contextWindow != null
                    ? ` · ${c.contextWindow.toLocaleString()} ctx`
                    : ""}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </label>
  );
}
```

In `ModelRow`, replace the model-id `TextField` (currently `:843-848`):

```tsx
        <TextField
          label="Model id"
          value={draft.modelId}
          onChange={(v) => set({ modelId: v })}
          placeholder="claude-sonnet-4-6"
        />
```

with:

```tsx
        <ModelIdField draft={draft} set={set} />
```

- [ ] **Step 2: Typecheck + build**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/pages/SettingsPage.tsx
git commit -m "web: model-id autocomplete + limit prefill in settings"
```

---

### Task 7: E2E coverage + full gate

**Files:**
- Create: `clients/web/e2e/k-model-cards.spec.ts`

**Interfaces:**
- Consumes: everything above. The e2e harness (`global-setup.ts`) builds and runs the real `horsie-server`, so the bundled seed (Task 2) is present; tests navigate `appBase` and use the testids from Tasks 5–6. Bundled seed guarantees `claude-sonnet-4-6` with `contextWindow` 200000 and `maxTokens` 16384.

- [ ] **Step 1: The spec**

Create `clients/web/e2e/k-model-cards.spec.ts`:

```ts
// Model cards: admin-page CRUD over the seeded catalog, and the Settings
// model form's id autocomplete + limit prefill.

import { test, expect } from "./fixtures";

test.describe("model cards", () => {
  test("admin page lists seeded cards and supports CRUD", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin`);

    // Bundled seed is present (the real server seeds at startup).
    await expect(page.getByText("claude-sonnet-4-6").first()).toBeVisible();

    // Create.
    await page.getByTestId("add-model-card").click();
    const draft = page.getByTestId("model-card-row-new");
    await draft.getByLabel("Model id").fill("e2e-model-1");
    await draft.getByLabel("Name").fill("E2E Model");
    await draft.getByLabel("Context window (optional)").fill("123456");
    await draft.getByLabel("Max tokens (optional)").fill("4096");
    await draft.getByTestId("model-card-save").click();

    const row = page.getByTestId("model-card-row-e2e-model-1");
    await expect(row).toBeVisible();
    // model_id is immutable once saved.
    await expect(row.getByLabel("Model id")).toBeDisabled();

    // Edit the name.
    await row.getByLabel("Name").fill("E2E Model Renamed");
    await row.getByTestId("model-card-save").click();
    await expect(row.getByLabel("Name")).toHaveValue("E2E Model Renamed");

    // Persists across reload.
    await page.reload();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toBeVisible();

    // Delete (accept the confirm dialog).
    page.on("dialog", (d) => d.accept());
    await page
      .getByTestId("model-card-row-e2e-model-1")
      .getByTestId("model-card-remove")
      .click();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toHaveCount(0);
  });

  test("settings model form autocompletes model id and prefills limits", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings`);
    await page.getByRole("button", { name: "Add model" }).click();

    // The new row is appended last (global-setup already seeded one model).
    const idInput = page.getByTestId("model-id-input").last();
    await idInput.fill("claude-sonnet");

    const suggestion = page.getByTestId(
      "model-card-suggestion-claude-sonnet-4-6",
    );
    await expect(suggestion).toBeVisible();
    await suggestion.dispatchEvent("mousedown");

    await expect(idInput).toHaveValue("claude-sonnet-4-6");
    await expect(
      page.getByLabel("Context window (optional)").last(),
    ).toHaveValue("200000");
    await expect(page.getByLabel("Max tokens (optional)").last()).toHaveValue(
      "16384",
    );
  });
}
```

Note: the suggestion button uses `onMouseDown`, so `dispatchEvent("mousedown")` is used instead of `click()` (Playwright's `click()` also works — mousedown fires first — but `dispatchEvent` is unambiguous).

- [ ] **Step 2: Run the new e2e spec**

Run: `cd clients/web && bun run test:e2e k-model-cards`
Expected: 2 passed. (This builds the Rust binaries and web assets via global-setup; it's slow the first time.)

- [ ] **Step 3: Run the full e2e suite**

Run: `cd clients/web && bun run test:e2e`
Expected: all specs pass (no regressions in a–j).

- [ ] **Step 4: Run the full pre-PR gate**

Run: `make check` (fmt-check + clippy -D warnings + workspace tests) and `make web-build`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add clients/web/e2e/k-model-cards.spec.ts
git commit -m "e2e: model cards admin + settings autocomplete"
```

---

## Self-Review Notes (resolved)

- **Spec coverage:** table+store (T1), bundled+file seeding (T2), both API surfaces + error codes (T3), TS codegen + client + hooks (T4), `/admin` page + sidebar (T5), Settings autocomplete/prefill (T6), store/HTTP/e2e tests (T1/T3/T7). Spec's "add/edit modal" was changed to inline rows per the codebase's `McpServerRow` idiom (no modal pattern exists in the client) — behavior identical.
- **Type consistency:** `ModelCard{,Input,Update}` identical across fluorite/Rust/TS; `ModelCardError` variants match the Task 3 `map_card_err` arms; hook names match Tasks 5–6 imports; testids match Task 7 selectors; seed entry `claude-sonnet-4-6`/200000/16384 matches the e2e assertion.
- **Deliberate deviations from spec:** inline rows instead of a modal (codebase idiom); `GET /api/model-cards` with absent prefix returns all cards (needed by both autocomplete-empty and as a cheap full list — still one endpoint).
