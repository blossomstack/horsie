//! The model-card catalog: reference records of well-known models (official
//! model id + token limits). Reference data, NOT runtime config — lives
//! outside `DbConfigStore`/`SettingsView`, and no registry rebuild is needed
//! when cards change. Seeded on an account's first touch (insert-if-missing),
//! managed via /api/settings/model-cards, searched via /api/model-cards.

use crate::db::Db;
use crate::projects::ProjectId;
use horsie_models::model_cards::{ModelCard, ModelCardInput, ModelCardUpdate};
use sqlx::Row;
use sqlx::any::AnyRow;

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
    db: Db,
    /// Bound once, here, rather than passed per call: there is then no call
    /// site that *can* hand a method the wrong account.
    user: ProjectId,
}

fn validate(
    model_id: &str,
    name: &str,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
    thinking_efforts: Option<&Vec<String>>,
    default_thinking_effort: Option<&str>,
    thinking_dialect: Option<&str>,
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
    validate_thinking(thinking_efforts, default_thinking_effort, thinking_dialect)
}

/// Whole-card validation, for the paths that hold a `ModelCardInput` already.
fn validate_card(c: &ModelCardInput) -> Result<(), ModelCardError> {
    validate(
        &c.model_id,
        &c.name,
        c.context_window,
        c.max_tokens,
        c.thinking_efforts.as_ref(),
        c.default_thinking_effort.as_deref(),
        c.thinking_dialect.as_deref(),
    )
}

/// The same rules `validate_model` applies to a configured model, applied to
/// the card that prefills one.
///
/// A card took arbitrary strings: `["banana","high"]` was a 201, and `banana`
/// was then injected into the Settings → Models dropdown as a *selected*
/// option with no checkbox able to unselect it — a value the model form could
/// not have produced and cannot repair.
fn validate_thinking(
    efforts: Option<&Vec<String>>,
    default_effort: Option<&str>,
    dialect: Option<&str>,
) -> Result<(), ModelCardError> {
    let offered: &[String] = efforts.map_or(&[][..], |v| v.as_slice());
    for e in offered {
        if horsie_agentcore::ThinkingEffort::parse(e).is_none() {
            return Err(ModelCardError::Invalid(format!(
                "unknown thinking effort '{e}'"
            )));
        }
    }
    if let Some(d) = dialect
        && horsie_agentcore::ThinkingDialect::parse(d).is_none()
    {
        return Err(ModelCardError::Invalid(format!(
            "unknown thinking dialect '{d}'"
        )));
    }
    if let Some(def) = default_effort
        && !offered.iter().any(|e| e == def)
    {
        return Err(ModelCardError::Invalid(format!(
            "default thinking effort '{def}' is not among the offered efforts"
        )));
    }
    Ok(())
}

const COLUMNS: &str = "model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect, base_url, forced_tools_disable_thinking, supports_images, supports_documents, created_at, updated_at";

fn row_to_card(r: &AnyRow) -> Result<ModelCard, sqlx::Error> {
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
        base_url: r.try_get("base_url")?,
        forced_tools_disable_thinking: Some(
            r.try_get::<i64, _>("forced_tools_disable_thinking")? != 0,
        ),
        supports_images: Some(r.try_get::<i64, _>("supports_images")? != 0),
        supports_documents: Some(r.try_get::<i64, _>("supports_documents")? != 0),
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

impl ModelCardStore {
    pub fn new(db: Db, user: ProjectId) -> Self {
        Self { db, user }
    }

    /// Every card, ordered by `model_id`.
    pub async fn list(&self) -> Result<Vec<ModelCard>, ModelCardError> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLUMNS} FROM model_cards WHERE project_id = ? ORDER BY model_id"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
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
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLUMNS} FROM model_cards WHERE project_id = ? AND model_id LIKE ? ESCAPE '\\' \
             ORDER BY model_id LIMIT ?"
        )))
        .bind(self.user.as_str())
        .bind(format!("{escaped}%"))
        .bind(SEARCH_LIMIT)
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        rows.iter()
            .map(row_to_card)
            .collect::<Result<_, _>>()
            .map_err(|e| ModelCardError::Db(e.to_string()))
    }

    pub async fn get(&self, model_id: &str) -> Result<Option<ModelCard>, ModelCardError> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLUMNS} FROM model_cards WHERE project_id = ? AND model_id = ?"
        )))
        .bind(self.user.as_str())
        .bind(model_id)
        .fetch_optional(self.db.pool())
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
            input.thinking_efforts.as_ref(),
            input.default_thinking_effort.as_deref(),
            input.thinking_dialect.as_deref(),
        )?;
        if self.get(&input.model_id).await?.is_some() {
            return Err(ModelCardError::Duplicate(format!(
                "model card '{}' already exists",
                input.model_id
            )));
        }
        sqlx::query(&self.db.q(
            "INSERT INTO model_cards (project_id, model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect, base_url, forced_tools_disable_thinking, supports_images, supports_documents) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(self.user.as_str())
        .bind(&input.model_id)
        .bind(&input.name)
        .bind(input.context_window.map(i64::from))
        .bind(input.max_tokens.map(i64::from))
        .bind(crate::config::store::encode_efforts(input.thinking_efforts.as_ref()))
        .bind(input.default_thinking_effort.clone())
        .bind(input.thinking_dialect.clone())
        .bind(input.base_url.clone())
        .bind(i64::from(input.forced_tools_disable_thinking.unwrap_or(false)))
        .bind(i64::from(input.supports_images.unwrap_or(false)))
        .bind(i64::from(input.supports_documents.unwrap_or(false)))
        .execute(self.db.pool())
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
            update.thinking_efforts.as_ref(),
            update.default_thinking_effort.as_deref(),
            update.thinking_dialect.as_deref(),
        )?;
        let statement = format!(
            "UPDATE model_cards SET name = ?, context_window = ?, max_tokens = ?, \
             thinking_efforts = ?, default_thinking_effort = ?, thinking_dialect = ?, \
             base_url = ?, forced_tools_disable_thinking = ?, \
             supports_images = ?, supports_documents = ?, \
             updated_at = {} WHERE project_id = ? AND model_id = ?",
            self.db.now_text()
        );
        let res = sqlx::query(&self.db.q(&statement))
            .bind(&update.name)
            .bind(update.context_window.map(i64::from))
            .bind(update.max_tokens.map(i64::from))
            .bind(crate::config::store::encode_efforts(
                update.thinking_efforts.as_ref(),
            ))
            .bind(update.default_thinking_effort.clone())
            .bind(update.thinking_dialect.clone())
            .bind(update.base_url.clone())
            .bind(i64::from(
                update.forced_tools_disable_thinking.unwrap_or(false),
            ))
            .bind(i64::from(update.supports_images.unwrap_or(false)))
            .bind(i64::from(update.supports_documents.unwrap_or(false)))
            .bind(self.user.as_str())
            .bind(model_id)
            .execute(self.db.pool())
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
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM model_cards WHERE project_id = ? AND model_id = ?"),
        )
        .bind(self.user.as_str())
        .bind(model_id)
        .execute(self.db.pool())
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
            validate_card(c)?;
            // `ON CONFLICT DO NOTHING` rather than SQLite's `INSERT OR IGNORE`:
            // same semantics, and the standard spelling works on both backends.
            let res = sqlx::query(&self.db.q(
                "INSERT INTO model_cards (project_id, model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect, base_url, forced_tools_disable_thinking, supports_images, supports_documents) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (project_id, model_id) DO NOTHING",
            ))
            .bind(self.user.as_str())
            .bind(&c.model_id)
            .bind(&c.name)
            .bind(c.context_window.map(i64::from))
            .bind(c.max_tokens.map(i64::from))
            .bind(crate::config::store::encode_efforts(c.thinking_efforts.as_ref()))
            .bind(c.default_thinking_effort.clone())
            .bind(c.thinking_dialect.clone())
            .bind(c.base_url.clone())
            .bind(i64::from(c.forced_tools_disable_thinking.unwrap_or(false)))
            .bind(i64::from(c.supports_images.unwrap_or(false)))
            .bind(i64::from(c.supports_documents.unwrap_or(false)))
            .execute(self.db.pool())
            .await
            .map_err(|e| ModelCardError::Db(e.to_string()))?;
            inserted += res.rows_affected() as usize;
        }
        Ok(inserted)
    }

    /// Seed this account's catalogue unless `marker` says it already has been.
    ///
    /// Called on the account's first touch rather than at boot, because looping
    /// the bundled catalogue over every account at every startup is O(accounts
    /// × cards) of writes before the port opens.
    ///
    /// The marker is a hash of the resolved seed set rather than the server
    /// version: an operator's `--model-cards-seed` file can change without a
    /// version bump, and a marker that misses that is a marker that lies. It
    /// lives in this account's own `settings` row, so an account that has never
    /// been touched carries no marker and gets seeded.
    pub async fn seed_once(
        &self,
        cards: &[ModelCardInput],
        marker: &str,
    ) -> Result<usize, ModelCardError> {
        let seen: Option<String> = sqlx::query_scalar(
            &self
                .db
                .q("SELECT value FROM settings WHERE project_id = ? AND key = ?"),
        )
        .bind(self.user.as_str())
        .bind(SEED_MARKER_KEY)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        if seen.as_deref() == Some(marker) {
            return Ok(0);
        }
        let inserted = self.seed_if_missing(cards).await?;
        sqlx::query(&self.db.q(
            "INSERT INTO settings (project_id, key, value) VALUES (?, ?, ?) \
             ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value",
        ))
        .bind(self.user.as_str())
        .bind(SEED_MARKER_KEY)
        .bind(marker)
        .execute(self.db.pool())
        .await
        .map_err(|e| ModelCardError::Db(e.to_string()))?;
        Ok(inserted)
    }
}

/// The `settings` key holding the hash of the seed set this account last got.
const SEED_MARKER_KEY: &str = "model_cards_seed";

/// A stable, short digest of a seed set, for [`ModelCardStore::seed_once`].
///
/// Computed once at boot and handed to every account's build. FNV-1a over the
/// serialized inputs: this is a change detector, not a security boundary, and
/// pulling in a cryptographic hash to compare a marker to itself would be
/// weight for nothing.
#[must_use]
pub fn seed_marker(cards: &[ModelCardInput]) -> String {
    let bytes = serde_json::to_vec(cards).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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
        validate_card(c).map_err(|e| match e {
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

    async fn test_store() -> ModelCardStore {
        ModelCardStore::new(crate::db::testing::db().await, ProjectId::new("1"))
    }

    /// The marker is what stops a boot-time loop becoming a per-request one:
    /// seed on first touch, then never again until the seed set itself changes.
    #[tokio::test]
    async fn seed_once_seeds_on_first_touch_and_not_again() {
        let store = test_store().await;
        let seeds = [input("m1", "One", Some(1000), None)];
        let marker = seed_marker(&seeds);

        assert_eq!(store.seed_once(&seeds, &marker).await.unwrap(), 1);
        assert_eq!(store.seed_once(&seeds, &marker).await.unwrap(), 0);

        // An operator deleting a bundled card gets it back only when the seed
        // set changes — which is the price of not having a shared catalogue.
        store.delete("m1").await.unwrap();
        assert_eq!(store.seed_once(&seeds, &marker).await.unwrap(), 0);

        let grown = [
            input("m1", "One", Some(1000), None),
            input("m2", "Two", Some(2000), None),
        ];
        assert_eq!(
            store.seed_once(&grown, &seed_marker(&grown)).await.unwrap(),
            2,
            "a changed seed set reseeds, and restores what was deleted"
        );
    }

    /// A marker derived from the server version would miss an operator editing
    /// their `--model-cards-seed` file, so it is derived from the set itself.
    #[test]
    fn the_marker_follows_the_seed_set_not_the_release() {
        let one = [input("m1", "One", Some(1000), None)];
        let renamed = [input("m1", "One Renamed", Some(1000), None)];
        assert_eq!(seed_marker(&one), seed_marker(&one));
        assert_ne!(seed_marker(&one), seed_marker(&renamed));
        assert_ne!(seed_marker(&one), seed_marker(&[]));
    }

    /// Each account gets its own copy: the catalogue is per-account precisely
    /// so a member can add a card the operator has not blessed.
    #[tokio::test]
    async fn each_account_is_seeded_separately() {
        let db = crate::db::testing::db().await;
        let (one, two) = (
            ModelCardStore::new(db.clone(), ProjectId::generate()),
            ModelCardStore::new(db, ProjectId::generate()),
        );
        let seeds = [input("m1", "One", Some(1000), None)];
        let marker = seed_marker(&seeds);

        assert_eq!(one.seed_once(&seeds, &marker).await.unwrap(), 1);
        assert_eq!(
            two.seed_once(&seeds, &marker).await.unwrap(),
            1,
            "the second account's marker is its own, and so is its catalogue"
        );
        one.delete("m1").await.unwrap();
        assert_eq!(two.list().await.unwrap().len(), 1);
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
            base_url: None,
            forced_tools_disable_thinking: None,
            supports_images: None,
            supports_documents: None,
        }
    }

    fn update_of(name: &str, cw: Option<u32>, mt: Option<u32>) -> ModelCardUpdate {
        ModelCardUpdate {
            name: name.into(),
            context_window: cw,
            max_tokens: mt,
            thinking_efforts: None,
            default_thinking_effort: None,
            thinking_dialect: None,
            base_url: None,
            forced_tools_disable_thinking: None,
            supports_images: None,
            supports_documents: None,
        }
    }

    #[tokio::test]
    async fn base_url_and_forced_tools_flag_round_trip() {
        let store = test_store().await;

        let mut card = input("ds", "DS", Some(1000), Some(100));
        card.base_url = Some("https://api.deepseek.com".into());
        card.forced_tools_disable_thinking = Some(true);
        let created = store.insert(&card).await.unwrap();
        assert_eq!(
            created.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(created.forced_tools_disable_thinking, Some(true));

        let fetched = store.get("ds").await.unwrap().unwrap();
        assert_eq!(
            fetched.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(fetched.forced_tools_disable_thinking, Some(true));

        let mut change = update_of("DS", Some(1000), Some(100));
        change.base_url = Some("https://proxy.example".into());
        change.forced_tools_disable_thinking = Some(false);
        let updated = store.update("ds", &change).await.unwrap();
        assert_eq!(updated.base_url.as_deref(), Some("https://proxy.example"));
        assert_eq!(updated.forced_tools_disable_thinking, Some(false));
    }

    /// The two vision flags are independent: a card may claim either, both or
    /// neither, and an update that turns one off must not take the other with
    /// it.
    #[tokio::test]
    async fn vision_flags_round_trip_independently() {
        let store = test_store().await;

        let mut card = input("seer", "Seer", None, None);
        card.supports_images = Some(true);
        let created = store.insert(&card).await.unwrap();
        assert_eq!(created.supports_images, Some(true));
        assert_eq!(created.supports_documents, Some(false));
        let fetched = store.get("seer").await.unwrap().unwrap();
        assert_eq!(fetched.supports_images, Some(true));
        assert_eq!(fetched.supports_documents, Some(false));

        let mut change = update_of("Seer", None, None);
        change.supports_documents = Some(true);
        let updated = store.update("seer", &change).await.unwrap();
        assert_eq!(updated.supports_images, Some(false));
        assert_eq!(updated.supports_documents, Some(true));
    }

    /// A card written before 0046 claims neither capability, and reads that
    /// way — the direction that withholds rather than the one that fails a
    /// turn.
    #[tokio::test]
    async fn a_card_without_the_vision_flags_reads_as_false() {
        let store = test_store().await;
        store
            .insert(&input("plain", "Plain", None, None))
            .await
            .unwrap();
        let c = store.get("plain").await.unwrap().unwrap();
        assert_eq!(c.supports_images, Some(false));
        assert_eq!(c.supports_documents, Some(false));
    }

    /// The seeded catalogue is where a fresh install learns which models can
    /// see, so the models the form prefills from must actually carry it.
    #[tokio::test]
    async fn bundled_seed_marks_the_models_that_can_see() {
        let cards = bundled_seed().expect("bundled seed parses");
        let of = |id: &str| {
            cards
                .iter()
                .find(|c| c.model_id == id)
                .unwrap_or_else(|| panic!("catalog includes {id}"))
                .clone()
        };
        for id in ["claude-sonnet-4-5", "claude-opus-4-5"] {
            assert_eq!(of(id).supports_images, Some(true), "{id} takes images");
            assert_eq!(of(id).supports_documents, Some(true), "{id} takes PDFs");
        }
        // OpenAI takes images on the chat wire this server speaks; PDFs go by
        // another route, so the card does not claim them.
        assert_eq!(of("gpt-4.1").supports_images, Some(true));
        assert_eq!(of("gpt-4.1").supports_documents, None);
    }

    /// Omitting the flag is legal and means false, so existing seed files and
    /// API clients keep working unchanged.
    #[tokio::test]
    async fn absent_flag_reads_back_as_false() {
        let store = test_store().await;
        store
            .insert(&input("plain", "Plain", None, None))
            .await
            .unwrap();
        let c = store.get("plain").await.unwrap().unwrap();
        assert_eq!(c.base_url, None);
        assert_eq!(c.forced_tools_disable_thinking, Some(false));
    }

    #[tokio::test]
    async fn crud_round_trip() {
        let store = test_store().await;

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
                &update_of("GPT-4o (2024)", Some(128_000), Some(16_384)),
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "GPT-4o (2024)");

        store.delete("gpt-4o").await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn insert_duplicate_is_rejected() {
        let store = test_store().await;
        store.insert(&input("a", "A", None, None)).await.unwrap();
        assert_eq!(
            store
                .insert(&input("a", "A2", None, None))
                .await
                .unwrap_err(),
            ModelCardError::Duplicate("model card 'a' already exists".into()),
        );
    }

    // `["banana","high"]` used to be a 201, and `banana` was then injected as a
    // *selected* option into the Settings → Models dropdown, with no checkbox
    // in the effort list able to unselect it.
    #[tokio::test]
    async fn thinking_values_outside_the_canonical_sets_are_rejected() {
        let store = test_store().await;

        let mut bad = input("m1", "M1", None, None);
        bad.thinking_efforts = Some(vec!["banana".into(), "high".into()]);
        let err = store.insert(&bad).await.unwrap_err();
        assert_eq!(
            err,
            ModelCardError::Invalid("unknown thinking effort 'banana'".into())
        );

        let mut bad = input("m1", "M1", None, None);
        bad.thinking_dialect = Some("banana_thinking".into());
        assert!(matches!(
            store.insert(&bad).await.unwrap_err(),
            ModelCardError::Invalid(_)
        ));

        // A default nothing offers is the same class of unusable data.
        let mut bad = input("m1", "M1", None, None);
        bad.thinking_efforts = Some(vec!["low".into()]);
        bad.default_thinking_effort = Some("high".into());
        assert!(matches!(
            store.insert(&bad).await.unwrap_err(),
            ModelCardError::Invalid(_)
        ));

        // And an update cannot smuggle in what an insert refuses.
        let mut good = input("m1", "M1", None, None);
        good.thinking_efforts = Some(vec!["low".into(), "high".into()]);
        good.default_thinking_effort = Some("high".into());
        good.thinking_dialect = Some("openai_effort".into());
        store.insert(&good).await.unwrap();

        let mut up = update_of("M1", None, None);
        up.thinking_efforts = Some(vec!["banana".into()]);
        assert!(matches!(
            store.update("m1", &up).await.unwrap_err(),
            ModelCardError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn validation_rejects_empty_ids_and_zero_limits() {
        let store = test_store().await;
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
        let store = test_store().await;
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
        let store = test_store().await;
        assert!(matches!(
            store
                .update("ghost", &update_of("x", None, None))
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
        let store = test_store().await;
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
        let store = test_store().await;
        let seeds = vec![input("a", "A", Some(1), None), input("b", "B", None, None)];
        assert_eq!(store.seed_if_missing(&seeds).await.unwrap(), 2);

        store
            .update("a", &update_of("A-edited", Some(999), None))
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
        let store = test_store().await;
        store
            .seed_if_missing(&bundled_seed().unwrap())
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
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

    #[tokio::test]
    async fn bundled_seed_carries_thinking_metadata() {
        let cards = bundled_seed().expect("bundled seed parses");

        let opus = cards
            .iter()
            .find(|c| c.model_id == "claude-opus-4-8")
            .expect("catalog includes claude-opus-4-8");
        assert_eq!(opus.context_window, Some(1_000_000));
        assert_eq!(opus.max_tokens, Some(128_000));
        assert_eq!(opus.thinking_dialect.as_deref(), Some("anthropic_effort"));
        assert_eq!(opus.default_thinking_effort.as_deref(), Some("high"));
        let efforts = opus.thinking_efforts.as_ref().expect("efforts listed");
        assert!(efforts.contains(&"xhigh".to_string()));
        assert!(efforts.contains(&"none".to_string()));

        // Fable 5 cannot disable thinking — offering `none` would produce a 400.
        let fable = cards
            .iter()
            .find(|c| c.model_id == "claude-fable-5")
            .expect("catalog includes claude-fable-5");
        assert_eq!(
            fable.thinking_dialect.as_deref(),
            Some("anthropic_always_on")
        );
        assert!(
            !fable
                .thinking_efforts
                .as_ref()
                .expect("efforts listed")
                .contains(&"none".to_string()),
            "Fable 5 must not offer `none`"
        );

        // xhigh arrived with Opus 4.7.
        let o46 = cards
            .iter()
            .find(|c| c.model_id == "claude-opus-4-6")
            .expect("catalog includes claude-opus-4-6");
        assert!(
            !o46.thinking_efforts
                .as_ref()
                .expect("efforts listed")
                .contains(&"xhigh".to_string()),
            "xhigh arrived with Opus 4.7"
        );
    }

    /// The migration adds the new columns and retires the superseded card.
    /// It deliberately does not write the replacement cards — those are new
    /// ids, so the bundled seed inserts them everywhere on the next boot.
    #[tokio::test]
    /// SQLite-only: the last assertion reads `pragma_table_info`. What it
    /// checks — that the migration adds columns and seeds nothing — is covered
    /// on PostgreSQL by every other test in this module running against a
    /// database these same migrations built.
    async fn migration_adds_the_new_columns_and_drops_deepseek_chat() {
        let db = crate::db::testing::sqlite().await;
        let pool = db.pool();

        let stale: Option<String> = sqlx::query_scalar(
            &db.q("SELECT model_id FROM model_cards WHERE model_id = 'deepseek-chat'"),
        )
        .fetch_optional(pool)
        .await
        .unwrap();
        assert!(stale.is_none(), "deepseek-chat must be gone");

        // A fresh database is still empty: nothing is seeded from a migration.
        let cards: i64 = sqlx::query_scalar(&db.q("SELECT count(*) FROM model_cards"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(cards, 0, "migrations must not seed the catalog");

        // The new columns exist and default correctly on both tables.
        sqlx::query(&db.q(
            "INSERT INTO model_cards (project_id, model_id, name) VALUES ('1', 'probe', 'Probe')",
        ))
        .execute(pool)
        .await
        .expect("insert without the new columns still works");
        let row = sqlx::query(&db.q(
            "SELECT base_url, forced_tools_disable_thinking FROM model_cards \
             WHERE project_id = '1' AND model_id = 'probe'",
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(row.get::<Option<String>, _>("base_url"), None);
        assert_eq!(row.get::<i64, _>("forced_tools_disable_thinking"), 0);

        let models_flag: i64 =
            sqlx::query_scalar(&db.q("SELECT count(*) FROM pragma_table_info('models') \
             WHERE name = 'forced_tools_disable_thinking'"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(models_flag, 1, "models must carry the flag too");
    }

    #[tokio::test]
    async fn bundled_seed_carries_the_deepseek_v4_cards() {
        let cards = bundled_seed().expect("bundled seed parses");
        assert!(
            !cards.iter().any(|c| c.model_id == "deepseek-chat"),
            "deepseek-chat is superseded and must not be seeded",
        );
        for id in ["deepseek-v4-flash", "deepseek-v4-pro"] {
            let c = cards
                .iter()
                .find(|c| c.model_id == id)
                .unwrap_or_else(|| panic!("catalog includes {id}"));
            assert_eq!(c.base_url.as_deref(), Some("https://api.deepseek.com"));
            assert_eq!(c.context_window, Some(1_048_576));
            assert_eq!(c.max_tokens, Some(393_216));
            assert_eq!(c.thinking_dialect.as_deref(), Some("openai_effort"));
            assert_eq!(c.default_thinking_effort.as_deref(), Some("high"));
            assert_eq!(c.forced_tools_disable_thinking, Some(true));
            assert_eq!(
                c.thinking_efforts
                    .as_ref()
                    .expect("efforts listed")
                    .as_slice(),
                ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
            );
        }
    }

    /// End-to-end: a fresh database plus the boot-time seed pass must produce
    /// usable DeepSeek cards, since the migration deliberately seeds nothing.
    #[tokio::test]
    async fn seeding_a_fresh_database_installs_the_deepseek_cards() {
        let store = test_store().await;
        store
            .seed_if_missing(&bundled_seed().unwrap())
            .await
            .unwrap();

        let flash = store.get("deepseek-v4-flash").await.unwrap().unwrap();
        assert_eq!(flash.base_url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(flash.forced_tools_disable_thinking, Some(true));
        assert!(store.get("deepseek-chat").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bundled_seed_efforts_and_dialects_are_canonical() {
        for c in bundled_seed().expect("bundled seed parses") {
            if let Some(d) = c.thinking_dialect.as_deref() {
                assert!(
                    horsie_agentcore::ThinkingDialect::parse(d).is_some(),
                    "{}: unknown dialect {d}",
                    c.model_id
                );
            }
            let efforts = c.thinking_efforts.clone().unwrap_or_default();
            for e in &efforts {
                assert!(
                    horsie_agentcore::ThinkingEffort::parse(e).is_some(),
                    "{}: unknown effort {e}",
                    c.model_id
                );
            }
            if let Some(def) = c.default_thinking_effort.as_deref() {
                assert!(
                    efforts.iter().any(|e| e == def),
                    "{}: default {def} not among offered efforts",
                    c.model_id
                );
            }
            if let Some(d) = c.thinking_dialect.as_deref() {
                let dialect = horsie_agentcore::ThinkingDialect::parse(d).expect("checked above");
                for e in &efforts {
                    let effort = horsie_agentcore::ThinkingEffort::parse(e).expect("checked above");
                    assert!(
                        dialect.supports(effort),
                        "{}: dialect {d} cannot express effort {e}",
                        c.model_id
                    );
                }
            }
        }
    }
}
