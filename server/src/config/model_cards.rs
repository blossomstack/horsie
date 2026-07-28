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
    if model_id != model_id.trim() || name != name.trim() {
        return Err(ModelCardError::Invalid(
            "model_id and name must not have leading or trailing whitespace".into(),
        ));
    }
    if context_window == Some(0) || max_tokens == Some(0) {
        return Err(ModelCardError::Invalid(
            "context_window and max_tokens must be positive".into(),
        ));
    }
    Ok(())
}

const COLUMNS: &str = "model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect, created_at, updated_at";

fn row_to_card(r: &sqlx::sqlite::SqliteRow) -> Result<ModelCard, sqlx::Error> {
    let cw: Option<i64> = r.try_get("context_window")?;
    let mt: Option<i64> = r.try_get("max_tokens")?;
    Ok(ModelCard {
        model_id: r.try_get("model_id")?,
        name: r.try_get("name")?,
        context_window: cw.and_then(|v| u32::try_from(v).ok()),
        max_tokens: mt.and_then(|v| u32::try_from(v).ok()),
        thinking_efforts: crate::config::store::decode_efforts(
            r.try_get::<Option<String>, _>("thinking_efforts")?
                .as_deref(),
        ),
        default_thinking_effort: r.try_get("default_thinking_effort")?,
        thinking_dialect: r.try_get("thinking_dialect")?,
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
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM model_cards ORDER BY model_id"
        ))
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
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM model_cards WHERE model_id = ?"
        ))
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
        validate(
            &input.model_id,
            &input.name,
            input.context_window,
            input.max_tokens,
        )?;
        if self.get(&input.model_id).await?.is_some() {
            return Err(ModelCardError::Duplicate(format!(
                "model card '{}' already exists",
                input.model_id
            )));
        }
        sqlx::query(
            "INSERT INTO model_cards (model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&input.model_id)
        .bind(&input.name)
        .bind(input.context_window.map(i64::from))
        .bind(input.max_tokens.map(i64::from))
        .bind(crate::config::store::encode_efforts(input.thinking_efforts.as_ref()))
        .bind(input.default_thinking_effort.clone())
        .bind(input.thinking_dialect.clone())
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
        validate(
            model_id,
            &update.name,
            update.context_window,
            update.max_tokens,
        )?;
        let res = sqlx::query(
            "UPDATE model_cards SET name = ?, context_window = ?, max_tokens = ?, \
             thinking_efforts = ?, default_thinking_effort = ?, thinking_dialect = ?, \
             updated_at = datetime('now') WHERE model_id = ?",
        )
        .bind(&update.name)
        .bind(update.context_window.map(i64::from))
        .bind(update.max_tokens.map(i64::from))
        .bind(crate::config::store::encode_efforts(
            update.thinking_efforts.as_ref(),
        ))
        .bind(update.default_thinking_effort.clone())
        .bind(update.thinking_dialect.clone())
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
    pub async fn seed_if_missing(&self, cards: &[ModelCardInput]) -> Result<usize, ModelCardError> {
        let mut inserted = 0usize;
        for c in cards {
            validate(&c.model_id, &c.name, c.context_window, c.max_tokens)?;
            let res = sqlx::query(
                "INSERT OR IGNORE INTO model_cards (model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&c.model_id)
            .bind(&c.name)
            .bind(c.context_window.map(i64::from))
            .bind(c.max_tokens.map(i64::from))
            .bind(crate::config::store::encode_efforts(c.thinking_efforts.as_ref()))
            .bind(c.default_thinking_effort.clone())
            .bind(c.thinking_dialect.clone())
            .execute(&self.pool)
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
            inserted += res.rows_affected() as usize;
        }
        Ok(inserted)
    }
}

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
            other @ (ModelCardError::Duplicate(_)
            | ModelCardError::NotFound(_)
            | ModelCardError::Db(_)) => {
                format!("{other:?}")
            }
        })?;
    }
    Ok(cards)
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
            thinking_efforts: None,
            default_thinking_effort: None,
            thinking_dialect: None,
        }
    }

    #[tokio::test]
    async fn crud_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;

        let card = store
            .insert(&input("gpt-4o", "GPT-4o", Some(128_000), Some(16_384)))
            .await
            .unwrap();
        assert_eq!(card.model_id, "gpt-4o");
        assert!(!card.created_at.is_empty());

        assert_eq!(store.get("gpt-4o").await.unwrap().unwrap().name, "GPT-4o");
        assert!(store.get("nope").await.unwrap().is_none());

        let updated = store
            .update(
                "gpt-4o",
                &ModelCardUpdate {
                    name: "GPT-4o (2024)".into(),
                    context_window: Some(128_000),
                    max_tokens: Some(16_384),
                    thinking_efforts: None,
                    default_thinking_effort: None,
                    thinking_dialect: None,
                },
            )
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
            store
                .insert(&input("a", "A2", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Duplicate("model card 'a' already exists".into()),
        );
    }

    #[tokio::test]
    async fn validation_rejects_empty_ids_and_zero_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        assert!(matches!(
            store
                .insert(&input("  ", "A", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store
                .insert(&input("a", " ", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store
                .insert(&input("a", "A", Some(0), None))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store
                .insert(&input("a", "A", None, Some(0)))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
    }

    #[tokio::test]
    async fn untrimmed_ids_and_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        assert!(matches!(
            store
                .insert(&input(" gpt-4o", "GPT-4o", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store
                .insert(&input("gpt-4o", "GPT-4o ", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(matches!(
            store
                .seed_if_missing(&[input("a ", "A", None, None)])
                .await
                .unwrap_err(),
            ModelCardError::Invalid(_),
        ));
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_and_delete_of_unknown_card_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        assert!(matches!(
            store
                .update(
                    "ghost",
                    &ModelCardUpdate {
                        name: "x".into(),
                        context_window: None,
                        max_tokens: None,
                        thinking_efforts: None,
                        default_thinking_effort: None,
                        thinking_dialect: None,
                    }
                )
                .await
                .unwrap_err(),
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
        store
            .insert(&input("gpt-4o", "GPT-4o", None, None))
            .await
            .unwrap();
        store
            .insert(&input("gpt-4.1", "GPT-4.1", None, None))
            .await
            .unwrap();
        store
            .insert(&input("claude-sonnet-4-6", "Sonnet", None, None))
            .await
            .unwrap();
        store
            .insert(&input("50%_off", "Wildcard", None, None))
            .await
            .unwrap();

        let hits = store.search_by_prefix("gpt-4").await.unwrap();
        assert_eq!(
            hits.iter().map(|c| c.model_id.as_str()).collect::<Vec<_>>(),
            ["gpt-4.1", "gpt-4o"]
        );

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
            .update(
                "a",
                &ModelCardUpdate {
                    name: "A-edited".into(),
                    context_window: Some(999),
                    max_tokens: None,
                    thinking_efforts: None,
                    default_thinking_effort: None,
                    thinking_dialect: None,
                },
            )
            .await
            .unwrap();

        // Reseeding (as happens every boot) inserts nothing and preserves the edit.
        assert_eq!(store.seed_if_missing(&seeds).await.unwrap(), 0);
        let a = store.get("a").await.unwrap().unwrap();
        assert_eq!(a.name, "A-edited");
        assert_eq!(a.context_window, Some(999));
    }

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
        store
            .seed_if_missing(&bundled_seed().unwrap())
            .await
            .unwrap();

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
}
