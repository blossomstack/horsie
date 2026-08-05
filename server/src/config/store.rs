//! Database-backed [`ConfigStore`]. Owns the settings database, builds the live
//! provider registry and the runtime vendors from it, and applies edits:
//! provider/model/default-vendor changes swap the live registry (next turn
//! sees them); vendor changes reconcile the live vendor map immediately — an
//! active vendor is reconfigured in place, a new/previously-inactive one is
//! built. No vendor edit needs a restart: velos vendors share the server-wide
//! runtime-connection registry, so there is no per-vendor listener to rebind.
//!
//! Vendors are generic — a `vendors(name, kind, config)` table plus a
//! kind-tagged config union — so a new vendor kind is a new match arm, not a
//! schema change. The database itself is SQLite or PostgreSQL, selected by
//! `database.url`; see `crate::db`.

use crate::config::ConfigStore;
use crate::db::Db;
use crate::sessions::spec::{SharedProviderRegistry, SharedVendors};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Secret, ThinkingDialect, ThinkingEffort};
use horsie_anthropic::AnthropicProvider;
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VendorCapabilities,
    VendorView,
};
use horsie_openai::OpenAiProvider;
use horsie_openai_responses::ResponsesProvider;
use horsie_openai_responses::chatgpt::{ChatGptTokens, DEFAULT_ISSUER, StoredTokens, TokenStore};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
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
    pub db: Db,
}

pub struct DbConfigStore {
    db: Db,
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
        Self::open_with(db_url, DEFAULT_MAX_CONNECTIONS, deps).await
    }

    /// As [`open`](Self::open), with an explicit pool size.
    pub async fn open_with(
        db_url: &str,
        max_connections: u32,
        deps: StoreDeps,
    ) -> Result<OpenedConfig, String> {
        Self::open_on(Db::open(db_url, max_connections).await?, deps).await
    }

    /// Build the store on an already-open database.
    ///
    /// The seam tests use, so they exercise whichever backend the run selected
    /// rather than a hardcoded SQLite URL.
    pub async fn open_on(db: Db, deps: StoreDeps) -> Result<OpenedConfig, String> {
        let provs = read_providers(&db, db.pool())
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&db, db.pool())
            .await
            .map_err(|e| e.to_string())?;
        let chatgpt = live_chatgpt_tokens(
            &db,
            read_provider_oauth(&db, db.pool())
                .await
                .map_err(|e| e.to_string())?,
        );
        let registry: SharedProviderRegistry =
            Arc::new(RwLock::new(build_registry(&provs, &mods, &chatgpt)?));

        // The server builds no vendors: every vendor is an agent that dials in
        // and publishes itself into this map. It starts empty at boot and is
        // never repopulated from the database.
        let vendors: SharedVendors = Arc::new(RwLock::new(HashMap::new()));

        // Kept as a preference even when no agent has connected yet — an agent
        // announcing this name later makes it take effect, so validating it
        // against the (empty) live map at boot would be wrong.
        let default_vendor = read_setting(&db, db.pool(), "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());

        let store = Arc::new(Self {
            db: db.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            info: deps.info,
        });
        Ok(OpenedConfig {
            store,
            registry,
            vendors,
            db,
        })
    }

    async fn build_view(&self) -> Result<SettingsView, String> {
        let provs = read_providers(&self.db, self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&self.db, self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
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
        let mut tx = self.db.pool().begin().await.map_err(|e| e.to_string())?;

        if let Some(providers) = &update.providers {
            let existing = read_providers(&self.db, &mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            let keep: HashMap<&str, &str> = existing
                .iter()
                .filter_map(|r| r.api_key.as_deref().map(|k| (r.name.as_str(), k)))
                .collect();
            let mut seen = HashSet::new();
            sqlx::query(&self.db.q("DELETE FROM providers"))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            for p in providers {
                let name = p.name.trim();
                if name.is_empty() {
                    return Err("provider name cannot be empty".into());
                }
                if !matches!(
                    p.kind.as_str(),
                    "anthropic" | "openai" | "openai-responses" | "chatgpt"
                ) {
                    return Err(format!(
                        "unsupported provider kind '{}' (expected 'anthropic', 'openai', \
                         'openai-responses' or 'chatgpt')",
                        p.kind
                    ));
                }
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate provider '{name}'"));
                }
                let api_key = resolve_secret(&p.api_key, keep.get(name).copied());
                sqlx::query(&self.db.q(
                    "INSERT INTO providers (name, kind, base_url, api_key, keep_thinking_signature) VALUES (?, ?, ?, ?, ?)",
                ))
                .bind(name)
                .bind(&p.kind)
                .bind(trimmed(&p.base_url))
                .bind(api_key)
                .bind(i64::from(p.keep_thinking_signature.unwrap_or(false)))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }

            // Providers are rewritten wholesale, so a removed or renamed one
            // would otherwise leave its sign-in behind — a live refresh token
            // belonging to nothing, which a later provider of the same name
            // would silently inherit.
            let orphans: Vec<String> = read_provider_oauth(&self.db, &mut *tx)
                .await
                .map_err(|e| e.to_string())?
                .into_keys()
                .filter(|p| !seen.contains(p))
                .collect();
            for provider in orphans {
                sqlx::query(&self.db.q("DELETE FROM provider_oauth WHERE provider = ?"))
                    .bind(&provider)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }

        if let Some(models) = &update.models {
            let mut seen = HashSet::new();
            sqlx::query(&self.db.q("DELETE FROM models"))
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
                sqlx::query(&self.db.q(
                    "INSERT INTO models (alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect, forced_tools_disable_thinking) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ))
                .bind(alias)
                .bind(&m.provider)
                .bind(m.model_id.trim())
                .bind(m.max_tokens.map(i64::from))
                .bind(context_window.map(i64::from))
                .bind(encode_efforts(m.thinking_efforts.as_ref()))
                .bind(m.thinking_effort.clone())
                .bind(m.thinking_dialect.clone())
                .bind(i64::from(m.forced_tools_disable_thinking.unwrap_or(false)))
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
            sqlx::query(&self.db.q(
                "INSERT INTO settings (key, value) VALUES ('default_vendor', ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            ))
            .bind(dv)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }

        // Validate providers/models by building the registry from the new state
        // before committing — a bad edit rolls back untouched.
        let provs = read_providers(&self.db, &mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&self.db, &mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let chatgpt = live_chatgpt_tokens(
            &self.db,
            read_provider_oauth(&self.db, &mut *tx)
                .await
                .map_err(|e| e.to_string())?,
        );
        let new_registry = build_registry(&provs, &mods, &chatgpt)?;

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
    forced_tools_disable_thinking: bool,
}

fn default_context_window(model_id: &str) -> Option<u32> {
    const TABLE: &[(&str, u32)] = &[
        ("claude-", 200_000),
        ("gpt-4o", 128_000),
        ("gpt-4.1", 1_000_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("deepseek", 1_048_576),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| model_id.contains(needle))
        .map(|(_, window)| *window)
}

// ── building providers + vendors ─────────────────────────────────────────────

/// Build the model→provider registry. Keyed by model alias, so each model's
/// provider is resolved and an Anthropic client built with its credentials.
fn build_registry(
    providers: &[ProviderRow],
    models: &[ModelRow],
    chatgpt: &HashMap<String, Arc<ChatGptTokens>>,
) -> Result<Registry, String> {
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
            // Only the OpenAI wire takes the forced-tools flag: Anthropic
            // accepts a pinned tool_choice with thinking enabled, so there is
            // nothing to reconcile there.
            "openai" => build_openai(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                dialect,
                m.forced_tools_disable_thinking,
            )?,
            "openai-responses" => build_responses(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                dialect,
            )?,
            "chatgpt" => {
                let tokens = chatgpt.get(p.name.as_str()).ok_or_else(|| {
                    format!(
                        "provider '{}' is a ChatGPT plan but has no sign-in yet — \
                         sign in from settings before using its models",
                        p.name
                    )
                })?;
                build_chatgpt(
                    tokens.clone(),
                    p.base_url.as_deref(),
                    &m.model_id,
                    max_tokens,
                    dialect,
                )?
            }
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
    forced_tools_disable_thinking: bool,
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
        .with_thinking_dialect(thinking_dialect)
        .with_forced_tools_disable_thinking(forced_tools_disable_thinking);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

fn build_responses(
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
        Some(k) => ResponsesProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => ResponsesProvider::new().map_err(|e| e.to_string())?,
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

/// A provider that spends a ChatGPT subscription.
///
/// Every model on the same provider shares one `Arc<ChatGptTokens>`, so a
/// refresh triggered by one model's turn is immediately visible to the others —
/// otherwise each model would refresh separately and they would race to rotate
/// the same refresh token.
fn build_chatgpt(
    tokens: Arc<ChatGptTokens>,
    base_url: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    thinking_dialect: ThinkingDialect,
) -> Result<Arc<dyn LlmProvider>, String> {
    let mut p = ResponsesProvider::with_chatgpt(tokens)
        .map_err(|e| e.to_string())?
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_thinking_dialect(thinking_dialect);
    // Overriding the Codex backend is for tests and for a proxy in front of it;
    // an ordinary deployment leaves it unset.
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

/// Persists refreshed ChatGPT tokens back into `provider_oauth`.
///
/// The provider refreshes on its own schedule, so this is the only writer that
/// runs outside a settings edit.
struct DbTokenStore {
    db: Db,
    provider: String,
}

// `Db` is not `Debug`, and the trait needs one. Naming the provider is all a
// log line wants anyway — the pool is not interesting and the tokens must never
// be printed.
impl std::fmt::Debug for DbTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbTokenStore")
            .field("provider", &self.provider)
            .finish()
    }
}

#[async_trait]
impl TokenStore for DbTokenStore {
    async fn save(&self, tokens: &StoredTokens) -> Result<(), String> {
        write_provider_oauth(&self.db, &self.provider, tokens)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Upsert one provider's credential.
pub(crate) async fn write_provider_oauth(
    db: &Db,
    provider: &str,
    tokens: &StoredTokens,
) -> Result<(), sqlx::Error> {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    sqlx::query(&db.q(
        "INSERT INTO provider_oauth (provider, access, refresh, expires_at, account_id, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(provider) DO UPDATE SET access = excluded.access, \
         refresh = excluded.refresh, expires_at = excluded.expires_at, \
         account_id = excluded.account_id, updated_at = excluded.updated_at",
    ))
    .bind(provider)
    .bind(&tokens.access)
    .bind(&tokens.refresh)
    .bind(tokens.expires_at)
    .bind(&tokens.account_id)
    .bind(now)
    .execute(db.pool())
    .await
    .map(|_| ())
}

/// Build the live `ChatGptTokens` for every stored credential.
fn live_chatgpt_tokens(
    db: &Db,
    stored: HashMap<String, StoredTokens>,
) -> HashMap<String, Arc<ChatGptTokens>> {
    stored
        .into_iter()
        .map(|(provider, tokens)| {
            let store = Arc::new(DbTokenStore {
                db: db.clone(),
                provider: provider.clone(),
            });
            (
                provider,
                Arc::new(ChatGptTokens::new(tokens, store, DEFAULT_ISSUER)),
            )
        })
        .collect()
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
        forced_tools_disable_thinking: Some(r.forced_tools_disable_thinking),
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

/// Default pool size. Sized for one server process sharing the pool between
/// settings reads and journal writes.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 10;

// These take the `Db` for its dialect and the executor separately, because the
// caller is sometimes a pool and sometimes an open transaction — the dialect is
// a property of the database, not of whichever handle is running the statement.
/// Load every stored OAuth credential, keyed by provider name.
///
/// Read as a batch before the registry is built so that `build_registry` stays
/// synchronous — it is called inside a transaction during an edit, where an
/// await on a second connection would be a deadlock waiting to happen.
async fn read_provider_oauth<'e, E>(
    db: &Db,
    ex: E,
) -> Result<HashMap<String, StoredTokens>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q("SELECT provider, access, refresh, expires_at, account_id FROM provider_oauth");
    let rows = sqlx::query(&sql).fetch_all(ex).await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        out.insert(
            r.try_get::<String, _>("provider")?,
            StoredTokens {
                access: r.try_get("access")?,
                refresh: r.try_get("refresh")?,
                expires_at: r.try_get("expires_at")?,
                account_id: r.try_get("account_id")?,
            },
        );
    }
    Ok(out)
}

async fn read_providers<'e, E>(db: &Db, ex: E) -> Result<Vec<ProviderRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q(
        "SELECT name, kind, base_url, api_key, keep_thinking_signature FROM providers ORDER BY name",
    );
    let rows = sqlx::query(&sql).fetch_all(ex).await?;
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

async fn read_models<'e, E>(db: &Db, ex: E) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q(
        "SELECT alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect, forced_tools_disable_thinking FROM models ORDER BY alias",
    );
    let rows = sqlx::query(&sql).fetch_all(ex).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ModelRow {
            forced_tools_disable_thinking: r.try_get::<i64, _>("forced_tools_disable_thinking")?
                != 0,
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

async fn read_setting<'e, E>(db: &Db, ex: E, key: &str) -> Result<Option<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q("SELECT value FROM settings WHERE key = ?");
    let row = sqlx::query(&sql).bind(key).fetch_optional(ex).await?;
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
            journal_backend: "file".into(),
        }
    }

    // The migration-version uniqueness check that used to live here now covers
    // both dialect directories, in `crate::db::tests`.
    async fn open() -> OpenedConfig {
        DbConfigStore::open_on(crate::db::testing::db().await, StoreDeps { info: info() })
            .await
            .unwrap()
    }

    /// The flag has to survive the save→read→build round trip, because it is
    /// what keeps a forced-handoff agent from 400ing on DeepSeek.
    #[tokio::test]
    async fn forced_tools_flag_persists_through_a_settings_update() {
        let store = open().await.store;

        store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "deepseek".into(),
                    kind: "openai".into(),
                    base_url: Some("https://api.deepseek.com".into()),
                    api_key: Some("k".into()),
                    keep_thinking_signature: None,
                }]),
                models: Some(vec![ModelInput {
                    alias: "ds".into(),
                    provider: "deepseek".into(),
                    model_id: "deepseek-v4-flash".into(),
                    max_tokens: Some(393_216),
                    context_window: None,
                    thinking_efforts: Some(vec!["none".into(), "high".into()]),
                    thinking_effort: Some("high".into()),
                    thinking_dialect: Some("openai_effort".into()),
                    forced_tools_disable_thinking: Some(true),
                }]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");

        let view = store.view().await.expect("view");
        let m = view.models.iter().find(|m| m.alias == "ds").expect("model");
        assert_eq!(m.forced_tools_disable_thinking, Some(true));
        // The built-in default for a "deepseek" model id is the real window.
        assert_eq!(m.context_window, Some(1_048_576));
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
            forced_tools_disable_thinking: None,
        }
    }

    #[tokio::test]
    async fn update_persists_and_swaps_registry() {
        let o = open().await;
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

    fn provider_of_kind(name: &str, kind: &str, key: Option<&str>) -> ProviderInput {
        ProviderInput {
            name: name.into(),
            kind: kind.into(),
            base_url: None,
            api_key: key.map(str::to_string),
            keep_thinking_signature: None,
        }
    }

    #[tokio::test]
    async fn an_api_key_responses_provider_builds() {
        let o = open().await;

        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind(
                    "p",
                    "openai-responses",
                    Some("sk-inline"),
                )]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    /// A ChatGPT provider is unusable until someone signs in, and the error has
    /// to say so — "unsupported kind" or a silent empty registry would send the
    /// operator looking in the wrong place.
    #[tokio::test]
    async fn a_chatgpt_provider_without_a_sign_in_is_rejected_with_a_useful_message() {
        let o = open().await;

        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind("p", "chatgpt", None)]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .expect_err("no credential yet");

        assert!(err.contains("sign in"), "unhelpful error: {err}");
    }

    #[tokio::test]
    async fn a_chatgpt_provider_builds_once_a_credential_is_stored() {
        let o = open().await;

        // A provider with no models: accepted, since nothing needs building yet.
        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind("p", "chatgpt", None)]),
                models: Some(vec![]),
                default_vendor: None,
            })
            .await
            .expect("provider alone is fine");

        write_provider_oauth(
            &o.db,
            "p",
            &StoredTokens {
                access: "a".into(),
                refresh: "r".into(),
                expires_at: 9_999_999_999,
                account_id: "acct_1".into(),
            },
        )
        .await
        .expect("stored");

        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind("p", "chatgpt", None)]),
                models: Some(vec![model("m", "p")]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    /// A credential must not outlive the provider it belongs to: a later
    /// provider reusing the name would otherwise silently inherit a live
    /// refresh token.
    #[tokio::test]
    async fn removing_a_provider_drops_its_sign_in() {
        let o = open().await;
        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind("p", "chatgpt", None)]),
                models: Some(vec![]),
                default_vendor: None,
            })
            .await
            .expect("update ok");
        write_provider_oauth(
            &o.db,
            "p",
            &StoredTokens {
                access: "a".into(),
                refresh: "r".into(),
                expires_at: 9_999_999_999,
                account_id: "acct_1".into(),
            },
        )
        .await
        .expect("stored");

        o.store
            .update(SettingsUpdate {
                providers: Some(vec![]),
                models: Some(vec![]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        let left = read_provider_oauth(&o.db, o.db.pool()).await.unwrap();
        assert!(
            left.is_empty(),
            "the orphaned credential survived: {left:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_provider_kind_is_still_rejected() {
        let o = open().await;

        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider_of_kind("p", "nonsense", None)]),
                models: Some(vec![]),
                default_vendor: None,
            })
            .await
            .expect_err("unknown kind");

        assert!(err.contains("unsupported provider kind"), "got: {err}");
    }

    #[tokio::test]
    async fn context_window_defaults_for_known_models_and_stays_editable() {
        let o = open().await;
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
                        forced_tools_disable_thinking: None,
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
                        forced_tools_disable_thinking: None,
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
        let o = open().await;
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
        let o = open().await;
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
        let o = open().await;
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
        let o = open().await;
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

    /// SQLite-only: it asserts on `pragma_table_info`, and it exists because
    /// SQLite's `DROP COLUMN` is recent enough to be worth pinning. The
    /// PostgreSQL mirror of 0006 is plain standard DDL with nothing to pin.
    #[tokio::test]
    async fn migration_0006_drops_api_key_env_and_preserves_rows() {
        // Deliberately unmigrated: the point is to build the pre-0006 schema
        // by hand and then apply exactly that one migration to it.
        let pool = &crate::db::testing::unmigrated_sqlite().await;

        // Mirror the pre-0006 `providers` shape (0001_init.sql).
        sqlx::query(
            "CREATE TABLE providers (
                name TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT,
                api_key_env TEXT, api_key TEXT)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO providers (name, kind, base_url, api_key_env, api_key) \
             VALUES ('p', 'anthropic', NULL, 'OLD_ENV_VAR', 'sk-inline')",
        )
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(include_str!(
            "../../migrations/sqlite/0006_drop_api_key_env.sql"
        ))
        .execute(pool)
        .await
        .expect("DROP COLUMN should succeed on the bundled sqlite");

        let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('providers')")
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap())
            .collect();
        assert!(!cols.iter().any(|c| c == "api_key_env"));

        let row = sqlx::query("SELECT name, api_key FROM providers WHERE name = 'p'")
            .fetch_one(pool)
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
        let o = open().await;

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
        let o = open().await;
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
        let o = open().await;
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
        let o = open().await;
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
        let o = open().await;
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

    // The WAL/synchronous pragmas moved to `Db::open`'s `after_connect` hook,
    // and so did the test that guards them: `crate::db::tests`.
}
