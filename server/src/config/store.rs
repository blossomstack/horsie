//! SQLite-backed [`ConfigStore`]. Owns the settings database, builds the live
//! provider registry and the runtime vendors from it, and applies edits:
//! provider/model/default-vendor changes swap the live registry (next turn
//! sees them); vendor changes reconcile the live vendor map immediately — an
//! active vendor is reconfigured in place, a new/previously-inactive one is
//! built. No vendor edit needs a restart: velos vendors share the server-wide
//! runtime-connection registry, so there is no per-vendor listener to rebind.
//!
//! Vendors are generic — a `vendors(name, kind, config)` table plus a
//! kind-tagged config union — so a new vendor kind is a new match arm, not a
//! schema change. `postgres` is a future driver swap behind the same code.

use crate::config::ConfigStore;
use crate::sessions::spec::{SharedProviderRegistry, SharedVendors};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Secret, ThinkingDialect, ThinkingEffort};
use horsie_anthropic::AnthropicProvider;
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VendorCapabilities,
    VendorView,
};
use horsie_openai::OpenAiProvider;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, RwLock};

type Registry = HashMap<String, Arc<dyn LlmProvider>>;

/// Deployment inputs the host supplies when opening the store.
pub struct StoreDeps {
    /// Read-only deployment paths, surfaced in the settings view.
    pub info: ServerInfo,
}

/// What [`DbConfigStore::open`] hands back: the store (for the HTTP layer) plus
/// the runtime objects the session supervisor needs.
pub struct OpenedConfig {
    pub store: Arc<DbConfigStore>,
    pub registry: SharedProviderRegistry,
    pub vendors: SharedVendors,
    /// The migrated connection pool, shared with feature stores (e.g. GitHub)
    /// that persist into the same settings DB.
    pub pool: SqlitePool,
}

pub struct DbConfigStore {
    pool: SqlitePool,
    registry: SharedProviderRegistry,
    /// The name new sessions prefer. A preference, not a validated reference:
    /// the agent that answers to it may connect long after boot.
    default_vendor: RwLock<String>,
    /// The live vendor roster, written by connected agents rather than by this
    /// store. Read here only to render the settings view.
    vendors: SharedVendors,
    info: ServerInfo,
}

impl DbConfigStore {
    /// Open (creating if absent) the database, run migrations, and build the
    /// live registry + vendors from it.
    pub async fn open(db_url: &str, deps: StoreDeps) -> Result<OpenedConfig, String> {
        let pool = open_pool(db_url).await?;

        let provs = read_providers(&pool).await.map_err(|e| e.to_string())?;
        let mods = read_models(&pool).await.map_err(|e| e.to_string())?;
        let registry: SharedProviderRegistry =
            Arc::new(RwLock::new(build_registry(&provs, &mods)?));

        // The server builds no vendors: every vendor is an agent that dials in
        // and publishes itself into this map. It starts empty at boot and is
        // never repopulated from the database.
        let vendors: SharedVendors = Arc::new(RwLock::new(HashMap::new()));

        // Kept as a preference even when no agent has connected yet — an agent
        // announcing this name later makes it take effect, so validating it
        // against the (empty) live map at boot would be wrong.
        let default_vendor = read_setting(&pool, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());

        let store = Arc::new(Self {
            pool: pool.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            info: deps.info,
        });
        Ok(OpenedConfig {
            store,
            registry,
            vendors,
            pool,
        })
    }

    async fn build_view(&self) -> Result<SettingsView, String> {
        let provs = read_providers(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&self.pool).await.map_err(|e| e.to_string())?;
        let default_vendor = self.default_vendor();
        Ok(SettingsView {
            providers: provs.iter().map(provider_view).collect(),
            models: mods.iter().map(model_view).collect(),
            vendors: self.vendors_view(&default_vendor),
            default_vendor,
            info: self.info.clone(),
            // Nothing the settings page can edit requires a restart any more.
            restart_required: false,
        })
    }

    /// The live vendor roster: whichever agents are connected right now, with
    /// the capabilities they announced. There is no configured-but-inactive
    /// state to report, so every entry here is by definition usable.
    fn vendors_view(&self, default_vendor: &str) -> Vec<VendorView> {
        let live = self.vendors.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<VendorView> = live
            .iter()
            .map(|(name, vendor)| VendorView {
                name: name.clone(),
                is_default: default_vendor == name.as_str(),
                capabilities: vendor_caps_view(vendor.capabilities()),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[async_trait]
impl ConfigStore for DbConfigStore {
    async fn view(&self) -> Result<SettingsView, String> {
        self.build_view().await
    }

    async fn update(&self, update: SettingsUpdate) -> Result<SettingsView, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if let Some(providers) = &update.providers {
            let existing = read_providers(&mut *tx).await.map_err(|e| e.to_string())?;
            let keep: HashMap<&str, &str> = existing
                .iter()
                .filter_map(|r| r.api_key.as_deref().map(|k| (r.name.as_str(), k)))
                .collect();
            let mut seen = HashSet::new();
            sqlx::query("DELETE FROM providers")
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            for p in providers {
                let name = p.name.trim();
                if name.is_empty() {
                    return Err("provider name cannot be empty".into());
                }
                if !matches!(p.kind.as_str(), "anthropic" | "openai") {
                    return Err(format!(
                        "unsupported provider kind '{}' (expected 'anthropic' or 'openai')",
                        p.kind
                    ));
                }
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate provider '{name}'"));
                }
                let api_key = resolve_secret(&p.api_key, keep.get(name).copied());
                sqlx::query(
                    "INSERT INTO providers (name, kind, base_url, api_key, keep_thinking_signature) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(name)
                .bind(&p.kind)
                .bind(trimmed(&p.base_url))
                .bind(api_key)
                .bind(i64::from(p.keep_thinking_signature.unwrap_or(false)))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
        }

        if let Some(models) = &update.models {
            let mut seen = HashSet::new();
            sqlx::query("DELETE FROM models")
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            for m in models {
                let alias = m.alias.trim();
                if alias.is_empty() {
                    return Err("model alias cannot be empty".into());
                }
                if m.model_id.trim().is_empty() {
                    return Err(format!("model '{alias}' needs a model id"));
                }
                if !seen.insert(alias.to_string()) {
                    return Err(format!("duplicate model '{alias}'"));
                }
                let context_window = m
                    .context_window
                    .or_else(|| default_context_window(m.model_id.trim()));
                if let Some(d) = m.thinking_dialect.as_deref()
                    && ThinkingDialect::parse(d).is_none()
                {
                    return Err(format!(
                        "model '{alias}' has unknown thinking dialect '{d}'"
                    ));
                }
                let offered: Vec<String> = m.thinking_efforts.clone().unwrap_or_default();
                for e in &offered {
                    if ThinkingEffort::parse(e).is_none() {
                        return Err(format!(
                            "model '{alias}' offers unknown thinking effort '{e}'"
                        ));
                    }
                }
                if let Some(def) = m.thinking_effort.as_deref()
                    && !offered.iter().any(|e| e == def)
                {
                    return Err(format!(
                        "model '{alias}' default thinking effort '{def}' is not among its offered efforts"
                    ));
                }
                sqlx::query(
                    "INSERT INTO models (alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(alias)
                .bind(&m.provider)
                .bind(m.model_id.trim())
                .bind(m.max_tokens.map(i64::from))
                .bind(context_window.map(i64::from))
                .bind(encode_efforts(m.thinking_efforts.as_ref()))
                .bind(m.thinking_effort.clone())
                .bind(m.thinking_dialect.clone())
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
        }

        if let Some(dv) = &update.default_vendor {
            if dv.trim().is_empty() {
                return Err("default vendor cannot be empty".into());
            }
            // Deliberately not validated against the live roster: the agent
            // answering to this name may connect long after the preference is
            // set, and rejecting it here would make the setting unusable
            // before its agent is running.
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES ('default_vendor', ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(dv)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }

        // Validate providers/models by building the registry from the new state
        // before committing — a bad edit rolls back untouched.
        let provs = read_providers(&mut *tx).await.map_err(|e| e.to_string())?;
        let mods = read_models(&mut *tx).await.map_err(|e| e.to_string())?;
        let new_registry = build_registry(&provs, &mods)?;

        tx.commit().await.map_err(|e| e.to_string())?;

        *self.registry.write().unwrap_or_else(|e| e.into_inner()) = new_registry;
        if let Some(dv) = &update.default_vendor {
            *self
                .default_vendor
                .write()
                .unwrap_or_else(|e| e.into_inner()) = dv.clone();
        }
        self.build_view().await
    }

    fn default_vendor(&self) -> String {
        self.default_vendor
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

// ── row types ────────────────────────────────────────────────────────────────

struct ProviderRow {
    name: String,
    kind: String,
    base_url: Option<String>,
    api_key: Option<String>,
    keep_thinking_signature: bool,
}

struct ModelRow {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<i64>,
    context_window: Option<i64>,
    thinking_efforts: Option<String>,
    thinking_effort: Option<String>,
    thinking_dialect: Option<String>,
}

fn default_context_window(model_id: &str) -> Option<u32> {
    const TABLE: &[(&str, u32)] = &[
        ("claude-", 200_000),
        ("gpt-4o", 128_000),
        ("gpt-4.1", 1_000_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("deepseek", 128_000),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| model_id.contains(needle))
        .map(|(_, window)| *window)
}

// ── building providers + vendors ─────────────────────────────────────────────

/// Build the model→provider registry. Keyed by model alias, so each model's
/// provider is resolved and an Anthropic client built with its credentials.
fn build_registry(providers: &[ProviderRow], models: &[ModelRow]) -> Result<Registry, String> {
    let by_name: HashMap<&str, &ProviderRow> =
        providers.iter().map(|p| (p.name.as_str(), p)).collect();
    let mut reg: Registry = HashMap::new();
    for m in models {
        let p = by_name.get(m.provider.as_str()).ok_or_else(|| {
            format!(
                "model '{}' references unknown provider '{}'",
                m.alias, m.provider
            )
        })?;
        let max_tokens = m.max_tokens.and_then(|v| u32::try_from(v).ok());
        let dialect = m
            .thinking_dialect
            .as_deref()
            .and_then(ThinkingDialect::parse)
            .unwrap_or(ThinkingDialect::NoControl);
        let built = match p.kind.as_str() {
            "anthropic" => build_anthropic(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                p.keep_thinking_signature,
                dialect,
            )?,
            "openai" => build_openai(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                dialect,
            )?,
            other => {
                return Err(format!(
                    "provider '{}' has unsupported kind '{other}'",
                    p.name
                ));
            }
        };
        reg.insert(m.alias.clone(), built);
    }
    Ok(reg)
}

fn build_anthropic(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    keep_thinking_signature: bool,
    thinking_dialect: ThinkingDialect,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key: Option<Secret> = match api_key {
        Some(k) if !k.is_empty() => Some(Secret::from(k)),
        Some(_) => return Err("inline api_key is empty".into()),
        None => None,
    };
    let mut p = match key {
        Some(k) => AnthropicProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => AnthropicProvider::new().map_err(|e| e.to_string())?,
    };
    p = p
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_keep_thinking_signature(keep_thinking_signature)
        .with_thinking_dialect(thinking_dialect);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

fn build_openai(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    thinking_dialect: ThinkingDialect,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key: Option<Secret> = match api_key {
        Some(k) if !k.is_empty() => Some(Secret::from(k)),
        Some(_) => return Err("inline api_key is empty".into()),
        None => None,
    };
    let mut p = match key {
        Some(k) => OpenAiProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => OpenAiProvider::new().map_err(|e| e.to_string())?,
    };
    p = p
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_thinking_dialect(thinking_dialect);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

// ── secret + value helpers ───────────────────────────────────────────────────

/// Write-only secret input: `None` keeps the stored value, `Some("")` clears,
/// `Some(v)` sets.
fn resolve_secret(input: &Option<String>, existing: Option<&str>) -> Option<String> {
    match input {
        None => existing.filter(|s| !s.is_empty()).map(str::to_string),
        Some(v) if !v.is_empty() => Some(v.clone()),
        Some(_) => None,
    }
}

/// A trimmed, non-empty value, else `None`.
fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ── projections ──────────────────────────────────────────────────────────────

/// Map a vendor's announced (domain) capabilities to the settings-wire view.
fn vendor_caps_view(caps: crate::runtime_vendor::VendorCapabilities) -> VendorCapabilities {
    VendorCapabilities {
        supports_provisioning: caps.supports_provisioning,
    }
}

fn provider_view(r: &ProviderRow) -> ProviderView {
    ProviderView {
        name: r.name.clone(),
        kind: r.kind.clone(),
        base_url: r.base_url.clone(),
        has_inline_key: r.api_key.as_deref().is_some_and(|s| !s.is_empty()),
        keep_thinking_signature: r.keep_thinking_signature,
    }
}

fn model_view(r: &ModelRow) -> ModelView {
    ModelView {
        alias: r.alias.clone(),
        provider: r.provider.clone(),
        model_id: r.model_id.clone(),
        max_tokens: r.max_tokens.and_then(|v| u32::try_from(v).ok()),
        context_window: r.context_window.and_then(|v| u32::try_from(v).ok()),
        thinking_efforts: decode_efforts(r.thinking_efforts.as_deref()),
        thinking_effort: r.thinking_effort.clone(),
        thinking_dialect: r.thinking_dialect.clone(),
    }
}

/// Efforts are stored as a JSON array in a TEXT column; a malformed value is
/// treated as absent rather than failing the whole settings read.
pub(crate) fn decode_efforts(raw: Option<&str>) -> Option<Vec<String>> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
}

pub(crate) fn encode_efforts(list: Option<&Vec<String>>) -> Option<String> {
    list.and_then(|v| serde_json::to_string(v).ok())
}

// ── connection + row reads ───────────────────────────────────────────────────

pub(crate) async fn open_pool(url: &str) -> Result<SqlitePool, String> {
    let opts = SqliteConnectOptions::from_str(url)
        .map_err(|e| format!("invalid database url '{url}': {e}"))?
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(opts)
        .await
        .map_err(|e| format!("open database '{url}': {e}"))?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .map_err(|e| format!("run migrations: {e}"))?;
    Ok(pool)
}

async fn read_providers<'e, E>(ex: E) -> Result<Vec<ProviderRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query(
        "SELECT name, kind, base_url, api_key, keep_thinking_signature FROM providers ORDER BY name",
    )
    .fetch_all(ex)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ProviderRow {
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            base_url: r.try_get("base_url")?,
            api_key: r.try_get("api_key")?,
            keep_thinking_signature: r.try_get::<i64, _>("keep_thinking_signature")? != 0,
        });
    }
    Ok(out)
}

async fn read_models<'e, E>(ex: E) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query(
        "SELECT alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect FROM models ORDER BY alias",
    )
    .fetch_all(ex)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ModelRow {
            alias: r.try_get("alias")?,
            provider: r.try_get("provider")?,
            model_id: r.try_get("model_id")?,
            max_tokens: r.try_get("max_tokens")?,
            context_window: r.try_get("context_window")?,
            thinking_efforts: r.try_get("thinking_efforts")?,
            thinking_effort: r.try_get("thinking_effort")?,
            thinking_dialect: r.try_get("thinking_dialect")?,
        });
    }
    Ok(out)
}

async fn read_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(Some(r.try_get("value")?)),
        None => Ok(None),
    }
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
    use horsie_models::settings::{ModelInput, ProviderInput};

    fn info() -> ServerInfo {
        ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        }
    }

    async fn open(dir: &std::path::Path) -> OpenedConfig {
        let _ = dir; // kept for signature symmetry with other test helpers in this crate
        DbConfigStore::open(
            &format!("sqlite://{}/t.db", dir.display()),
            StoreDeps { info: info() },
        )
        .await
        .unwrap()
    }

    fn provider(name: &str, key: Option<&str>) -> ProviderInput {
        ProviderInput {
            name: name.into(),
            kind: "anthropic".into(),
            base_url: Some("http://localhost:1".into()),
            api_key: key.map(str::to_string),
            keep_thinking_signature: None,
        }
    }

    fn model(alias: &str, provider: &str) -> ModelInput {
        ModelInput {
            alias: alias.into(),
            provider: provider.into(),
            model_id: "id".into(),
            max_tokens: None,
            context_window: None,
            thinking_efforts: None,
            thinking_effort: None,
            thinking_dialect: None,
        }
    }

    #[tokio::test]
    async fn update_persists_and_swaps_registry() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-inline"))]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .expect("update ok");
        assert_eq!(view.models.len(), 1);
        assert!(view.providers[0].has_inline_key);
        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    #[tokio::test]
    async fn context_window_defaults_for_known_models_and_stays_editable() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-inline"))]),
                models: Some(vec![
                    ModelInput {
                        alias: "sonnet".into(),
                        provider: "p".into(),
                        model_id: "claude-sonnet-4-6".into(),
                        max_tokens: None,
                        context_window: None,
                        thinking_efforts: None,
                        thinking_effort: None,
                        thinking_dialect: None,
                    },
                    ModelInput {
                        alias: "custom".into(),
                        provider: "p".into(),
                        model_id: "some-unknown-model".into(),
                        max_tokens: None,
                        context_window: Some(42_000),
                        thinking_efforts: None,
                        thinking_effort: None,
                        thinking_dialect: None,
                    },
                ]),
                default_vendor: None,
            })
            .await
            .expect("update ok");
        let sonnet = view.models.iter().find(|m| m.alias == "sonnet").unwrap();
        assert_eq!(sonnet.context_window, Some(200_000));
        let custom = view.models.iter().find(|m| m.alias == "custom").unwrap();
        assert_eq!(
            custom.context_window,
            Some(42_000),
            "an explicit value must never be overridden by the default table"
        );
    }

    #[tokio::test]
    async fn update_preserves_inline_key_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-secret"))]),
                models: None,
                default_vendor: None,
            })
            .await
            .unwrap();
        // Re-send without a key → keep it (the view still reports a stored key).
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", None)]),
                models: None,
                default_vendor: None,
            })
            .await
            .unwrap();
        assert!(view.providers[0].has_inline_key);
    }

    #[tokio::test]
    async fn update_rejects_unknown_provider_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("k"))]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .unwrap();
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![]),
                models: Some(vec![model("m", "ghost")]),
                default_vendor: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("ghost"), "error names the provider: {err}");
        // Rolled back: original provider+model survive, registry unchanged.
        let view = o.store.view().await.unwrap();
        assert_eq!(view.providers.len(), 1);
        assert_eq!(view.models.len(), 1);
        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    fn provider_kind(name: &str, kind: &str) -> ProviderInput {
        ProviderInput {
            name: name.into(),
            kind: kind.into(),
            base_url: Some("http://localhost:1".into()),
            api_key: Some("k".into()),
            keep_thinking_signature: None,
        }
    }

    #[tokio::test]
    async fn openai_provider_kind_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider_kind("local", "openai")]),
                models: Some(vec![model("m", "local")]),
                default_vendor: None,
            })
            .await
            .expect("openai must be an accepted provider kind");

        assert_eq!(view.providers.len(), 1);
        assert_eq!(view.providers[0].kind, "openai");
        // The model's provider was constructed and registered.
        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    #[tokio::test]
    async fn unknown_provider_kind_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider_kind("bogus", "cohere")]),
                models: None,
                default_vendor: None,
            })
            .await
            .expect_err("unknown kinds must be rejected");

        assert!(err.contains("cohere"), "error names the kind: {err}");
    }

    #[tokio::test]
    async fn migration_0006_drops_api_key_env_and_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/old.db", dir.path().display());
        let opts = SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();

        // Mirror the pre-0006 `providers` shape (0001_init.sql).
        sqlx::query(
            "CREATE TABLE providers (
                name TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT,
                api_key_env TEXT, api_key TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO providers (name, kind, base_url, api_key_env, api_key) \
             VALUES ('p', 'anthropic', NULL, 'OLD_ENV_VAR', 'sk-inline')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(include_str!("../../migrations/0006_drop_api_key_env.sql"))
            .execute(&pool)
            .await
            .expect("DROP COLUMN should succeed on the bundled sqlite");

        let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('providers')")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap())
            .collect();
        assert!(!cols.iter().any(|c| c == "api_key_env"));

        let row = sqlx::query("SELECT name, api_key FROM providers WHERE name = 'p'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("name").unwrap(), "p");
        assert_eq!(
            row.try_get::<Option<String>, _>("api_key")
                .unwrap()
                .as_deref(),
            Some("sk-inline")
        );
    }

    #[tokio::test]
    async fn keep_thinking_signature_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;

        // Defaults off for a fresh provider.
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("kimi", Some("sk-test"))]),
                models: Some(vec![model("m", "kimi")]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert!(!view.providers[0].keep_thinking_signature);

        // Opting in persists and reads back.
        let mut p = provider("real-anthropic", Some("sk-test"));
        p.keep_thinking_signature = Some(true);
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![p]),
                models: Some(vec![model("m", "real-anthropic")]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert!(view.providers[0].keep_thinking_signature);
    }

    #[tokio::test]
    async fn model_thinking_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let mut m = model("m", "p");
        m.thinking_efforts = Some(vec!["none".into(), "low".into(), "high".into()]);
        m.thinking_effort = Some("high".into());
        m.thinking_dialect = Some("anthropic_effort".into());
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        let got = &view.models[0];
        assert_eq!(
            got.thinking_efforts.clone().unwrap(),
            vec!["none".to_string(), "low".to_string(), "high".to_string()]
        );
        assert_eq!(got.thinking_effort.as_deref(), Some("high"));
        assert_eq!(got.thinking_dialect.as_deref(), Some("anthropic_effort"));
    }

    #[tokio::test]
    async fn model_thinking_config_defaults_to_absent() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert_eq!(view.models[0].thinking_efforts, None);
        assert_eq!(view.models[0].thinking_effort, None);
        assert_eq!(view.models[0].thinking_dialect, None);
    }

    #[tokio::test]
    async fn model_rejects_effort_outside_its_menu() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let mut m = model("m", "p");
        m.thinking_efforts = Some(vec!["low".into()]);
        m.thinking_effort = Some("max".into());
        m.thinking_dialect = Some("anthropic_effort".into());
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                default_vendor: None,
            })
            .await
            .expect_err("default effort must be one the model offers");
        assert!(
            err.contains("max"),
            "error should name the bad value: {err}"
        );
    }

    #[tokio::test]
    async fn model_rejects_unknown_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let mut m = model("m", "p");
        m.thinking_dialect = Some("telepathy".into());
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                default_vendor: None,
            })
            .await
            .expect_err("unknown dialect must be rejected");
        assert!(err.contains("telepathy"), "error should name it: {err}");
    }
}
