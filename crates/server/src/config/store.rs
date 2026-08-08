//! Database-backed [`ConfigStore`]. Owns the settings database, builds the live
//! provider registry from it, and applies edits: provider/model/default-vendor
//! changes swap the live registry, so the next turn sees them.
//!
//! It does **not** own runtime vendors. It holds the vendor map only to render
//! the settings view; the two things that write it are
//! [`RuntimeVendorRegistry`] (agents that dial in) and
//! [`RuntimeVendorConfigService`] (vendors configured in settings). Neither
//! needs a restart to take effect.
//!
//! The database itself is SQLite or PostgreSQL, selected by `database.url`; see
//! `crate::db`.
//!
//! [`RuntimeVendorRegistry`]: crate::runtime_vendor::RuntimeVendorRegistry
//! [`RuntimeVendorConfigService`]: crate::runtime_vendor::RuntimeVendorConfigService

use crate::auth::UserId;
use crate::config::ConfigStore;
use crate::db::Db;
use crate::sessions::spec::{RuntimeVendorMap, SharedProviderRegistry};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Secret, ThinkingDialect, ThinkingEffort};
use horsie_llm_providers::anthropic::AnthropicProvider;
use horsie_llm_providers::openai::OpenAiProvider;
use horsie_llm_providers::responses::ResponsesProvider;
use horsie_llm_providers::responses::chatgpt::{
    ChatGptTokens, ResponsesError, StoredTokens, TokenStore,
};
use horsie_models::settings::{
    ModelInput, ModelView, ProviderInput, ProviderView, ServerInfo, SettingsView,
    VendorCapabilities, VendorView,
};
use sqlx::Row;
use std::collections::HashMap;
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
    pub vendors: RuntimeVendorMap,
    /// Signs the dial-back token every runtime this account owns presents.
    ///
    /// Generated once and kept, rather than derived per runtime: rotating this
    /// one value invalidates every outstanding token at once, and there is no
    /// per-runtime row to migrate or expire.
    pub dial_secret: Arc<Vec<u8>>,
    /// The migrated connection pool, shared with feature stores (e.g. GitHub)
    /// that persist into the same settings DB.
    pub db: Db,
}

/// Test-only seeding, applying providers and then models one resource at a
/// time.
///
/// Exists because the tests predate the per-resource API and describe a whole
/// configuration at once, which is a fine way to *set up* a fixture even though
/// it is a bad way to expose an API.
#[cfg(test)]
impl DbConfigStore {
    pub(crate) async fn seed(
        &self,
        providers: Vec<ProviderInput>,
        models: Vec<ModelInput>,
    ) -> Result<SettingsView, String> {
        for p in providers {
            self.upsert_provider(p).await?;
        }
        for m in models {
            self.upsert_model(m).await?;
        }
        self.build_view().await
    }
}

/// The vendor sessions default to when no preference is stored.
pub const DEFAULT_VENDOR: &str = "local";

pub struct DbConfigStore {
    db: Db,
    /// Bound once, here, rather than passed per call: there is then no call
    /// site that *can* hand a method the wrong account.
    user: UserId,
    registry: SharedProviderRegistry,
    /// The name new sessions prefer. A preference, not a validated reference:
    /// the agent that answers to it may connect long after boot.
    default_vendor: RwLock<String>,
    /// The live vendor roster, written by connected agents rather than by this
    /// store. Read here only to render the settings view.
    vendors: RuntimeVendorMap,
    info: ServerInfo,
}

impl DbConfigStore {
    /// Open (creating if absent) the database, run migrations, and build the
    /// live registry + vendors from it.
    pub async fn open(db_url: &str, deps: StoreDeps, user: UserId) -> Result<OpenedConfig, String> {
        Self::open_with(db_url, DEFAULT_MAX_CONNECTIONS, deps, user).await
    }

    /// As [`open`](Self::open), with an explicit pool size.
    pub async fn open_with(
        db_url: &str,
        max_connections: u32,
        deps: StoreDeps,
        user: UserId,
    ) -> Result<OpenedConfig, String> {
        Self::open_on(Db::open(db_url, max_connections).await?, deps, user).await
    }

    /// Build the store on an already-open database.
    ///
    /// The seam tests use, so they exercise whichever backend the run selected
    /// rather than a hardcoded SQLite URL.
    pub async fn open_on(db: Db, deps: StoreDeps, user: UserId) -> Result<OpenedConfig, String> {
        let provs = read_providers(&db, db.pool(), &user)
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&db, db.pool(), &user)
            .await
            .map_err(|e| e.to_string())?;
        let chatgpt = live_chatgpt_tokens(
            &db,
            &user,
            read_provider_oauth(&db, db.pool(), &user)
                .await
                .map_err(|e| e.to_string())?,
        );
        let registry: SharedProviderRegistry =
            Arc::new(RwLock::new(build_registry(&provs, &mods, &chatgpt)?));

        // Empty here, and filled by its two writers: agents that dial in, and
        // `RuntimeVendorConfigService` replaying the `runtime_vendors` table.
        // This store never builds a vendor of its own.
        let vendors: RuntimeVendorMap = Arc::new(RwLock::new(HashMap::new()));

        let dial_secret = load_or_create_dial_secret(&db, &user).await?;

        // Kept as a preference even when no agent has connected yet — an agent
        // announcing this name later makes it take effect, so validating it
        // against the (empty) live map at boot would be wrong.
        let default_vendor = read_setting(&db, db.pool(), &user, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| DEFAULT_VENDOR.into());

        let store = Arc::new(Self {
            db: db.clone(),
            user,
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            info: deps.info,
        });
        Ok(OpenedConfig {
            store,
            registry,
            vendors,
            dial_secret,
            db,
        })
    }

    async fn build_view(&self) -> Result<SettingsView, String> {
        let provs = read_providers(&self.db, self.db.pool(), &self.user)
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&self.db, self.db.pool(), &self.user)
            .await
            .map_err(|e| e.to_string())?;
        // A ChatGPT plan's credential lives in `provider_oauth`, not in the
        // provider row, so the view cannot answer "is this usable" from
        // `providers` alone.
        let signed_in = read_provider_oauth(&self.db, self.db.pool(), &self.user)
            .await
            .map_err(|e| e.to_string())?;
        let default_vendor = self.default_vendor();
        Ok(SettingsView {
            providers: provs
                .iter()
                .map(|r| provider_view(r, signed_in.contains_key(&r.name)))
                .collect(),
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

impl DbConfigStore {
    /// Rebuild the registry from the transaction's pending state, commit, and
    /// swap the live registry in.
    ///
    /// This is the whole cross-resource validation story: building the registry
    /// is what rejects a model routed to a provider that does not exist, so
    /// every per-resource mutation gets that check by running this, and a bad
    /// edit rolls back untouched because the rebuild happens before the commit.
    async fn validate_and_commit(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<(), String> {
        let provs = read_providers(&self.db, &mut *tx, &self.user)
            .await
            .map_err(|e| e.to_string())?;
        let mods = read_models(&self.db, &mut *tx, &self.user)
            .await
            .map_err(|e| e.to_string())?;
        let chatgpt = live_chatgpt_tokens(
            &self.db,
            &self.user,
            read_provider_oauth(&self.db, &mut *tx, &self.user)
                .await
                .map_err(|e| e.to_string())?,
        );
        let new_registry = build_registry(&provs, &mods, &chatgpt)?;

        tx.commit().await.map_err(|e| e.to_string())?;
        *self.registry.write().unwrap_or_else(|e| e.into_inner()) = new_registry;
        Ok(())
    }
}

#[async_trait]
impl ConfigStore for DbConfigStore {
    async fn view(&self) -> Result<SettingsView, String> {
        self.build_view().await
    }

    async fn set_default_vendor(&self, vendor: &str) -> Result<SettingsView, String> {
        let vendor = vendor.trim();
        if vendor.is_empty() {
            return Err("default vendor cannot be empty".into());
        }

        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(&self.db.q(
            "INSERT INTO settings (user_id, key, value) VALUES (?, 'default_vendor', ?) \
             ON CONFLICT(user_id, key) DO UPDATE SET value = excluded.value",
        ))
        .bind(self.user.as_str())
        .bind(vendor)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;

        *self
            .default_vendor
            .write()
            .unwrap_or_else(|e| e.into_inner()) = vendor.to_string();
        self.build_view().await
    }

    async fn clear_default_vendor(&self) -> Result<SettingsView, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(
            &self
                .db
                .q("DELETE FROM settings WHERE user_id = ? AND key = 'default_vendor'"),
        )
        .bind(self.user.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;

        // Matches what `open` falls back to when the row is absent, so the
        // live value and a fresh boot agree.
        *self
            .default_vendor
            .write()
            .unwrap_or_else(|e| e.into_inner()) = DEFAULT_VENDOR.to_string();
        self.build_view().await
    }

    async fn rebuild_registry(&self) -> Result<(), String> {
        let tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        self.validate_and_commit(tx).await
    }

    async fn upsert_provider(&self, input: ProviderInput) -> Result<ProviderView, String> {
        let name = validate_provider(&input)?;

        // `begin_write` for the same reason `update` uses it: this reads the
        // stored key before rewriting the row, and a deferred transaction that
        // upgrades to a write that late loses to any writer that committed in
        // between.
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;

        let existing = read_providers(&self.db, &mut *tx, &self.user)
            .await
            .map_err(|e| e.to_string())?;
        let stored_key = existing
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| r.api_key.clone());

        let is_chatgpt = input.kind == "chatgpt";
        // A ChatGPT plan authorizes with an OAuth token and has no key field at
        // all, so any key on that row is a leftover from the kind it used to be.
        let api_key = if is_chatgpt {
            None
        } else {
            resolve_secret(&input.api_key, stored_key.as_deref())
        };

        sqlx::query(
            &self
                .db
                .q("DELETE FROM providers WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        sqlx::query(&self.db.q(
            "INSERT INTO providers (user_id, name, kind, base_url, api_key, keep_thinking_signature) VALUES (?, ?, ?, ?, ?, ?)",
        ))
        .bind(self.user.as_str())
        .bind(&name)
        .bind(&input.kind)
        .bind(trimmed(&input.base_url))
        .bind(api_key)
        .bind(i64::from(input.keep_thinking_signature.unwrap_or(false)))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        // A sign-in outlives the kind that created it. Wholesale rewrite got
        // this by deleting every row and keeping only the names that are
        // ChatGPT plans now; per-resource it has to be said out loud, or the
        // next provider of this name silently inherits a live refresh token.
        if !is_chatgpt {
            delete_provider_oauth(&self.db, &mut tx, &self.user, &name).await?;
        }

        self.validate_and_commit(tx).await?;

        self.build_view()
            .await?
            .providers
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("provider '{name}' vanished after write"))
    }

    async fn delete_provider(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;

        // Checked explicitly rather than left to the registry rebuild, so the
        // error names the models holding the provider open instead of saying
        // only that some model references something missing.
        let referencing: Vec<String> = read_models(&self.db, &mut *tx, &self.user)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|m| m.provider == name)
            .map(|m| m.alias)
            .collect();
        if !referencing.is_empty() {
            return Err(format!(
                "provider '{name}' is still used by model(s): {}",
                referencing.join(", ")
            ));
        }

        let affected = sqlx::query(
            &self
                .db
                .q("DELETE FROM providers WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if affected == 0 {
            return Err(format!("no such provider '{name}'"));
        }

        delete_provider_oauth(&self.db, &mut tx, &self.user, name).await?;

        self.validate_and_commit(tx).await
    }

    async fn upsert_model(&self, input: ModelInput) -> Result<ModelView, String> {
        let alias = validate_model(&input)?;
        let context_window = input
            .context_window
            .or_else(|| default_context_window(input.model_id.trim()));

        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;

        sqlx::query(
            &self
                .db
                .q("DELETE FROM models WHERE user_id = ? AND alias = ?"),
        )
        .bind(self.user.as_str())
        .bind(&alias)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        sqlx::query(&self.db.q(
            "INSERT INTO models (user_id, alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect, forced_tools_disable_thinking) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(self.user.as_str())
        .bind(&alias)
        .bind(&input.provider)
        .bind(input.model_id.trim())
        .bind(input.max_tokens.map(i64::from))
        .bind(context_window.map(i64::from))
        .bind(encode_efforts(input.thinking_efforts.as_ref()))
        .bind(input.thinking_effort.clone())
        .bind(input.thinking_dialect.clone())
        .bind(i64::from(input.forced_tools_disable_thinking.unwrap_or(false)))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        // The registry rebuild below is what rejects an unknown provider, and
        // it checks against the table rather than an incoming list — strictly
        // stronger than what the whole-document update could do.
        self.validate_and_commit(tx).await?;

        self.build_view()
            .await?
            .models
            .into_iter()
            .find(|m| m.alias == alias)
            .ok_or_else(|| format!("model '{alias}' vanished after write"))
    }

    async fn delete_model(&self, alias: &str) -> Result<(), String> {
        let alias = alias.trim();
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;

        let affected = sqlx::query(
            &self
                .db
                .q("DELETE FROM models WHERE user_id = ? AND alias = ?"),
        )
        .bind(self.user.as_str())
        .bind(alias)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if affected == 0 {
            return Err(format!("no such model '{alias}'"));
        }

        self.validate_and_commit(tx).await
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
                &p.name,
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
                &p.name,
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                dialect,
                m.forced_tools_disable_thinking,
            )?,
            "openai-responses" => build_responses(
                &p.name,
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

/// The key a provider row configures, or an error naming the provider.
///
/// A row without one is never allowed to fall through to the provider crate's
/// `new()`, because every one of those reads a key out of the process
/// environment — `OPENAI_API_KEY` directly, `ANTHROPIC_API_KEY` inside
/// `async-llm`'s client. That would have a provider the operator left blank
/// silently spend whatever credential the server happens to have inherited,
/// under a name that claims to carry none.
fn required_key(provider: &str, api_key: Option<&str>) -> Result<Secret, String> {
    match api_key {
        Some(k) if !k.is_empty() => Ok(Secret::from(k)),
        _ => Err(format!(
            "provider '{provider}' has no API key — add one in settings"
        )),
    }
}

fn build_anthropic(
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    keep_thinking_signature: bool,
    thinking_dialect: ThinkingDialect,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key = required_key(provider, api_key)?;
    let p = AnthropicProvider::with_api_key(key)
        .map_err(|e| e.to_string())?
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_keep_thinking_signature(keep_thinking_signature)
        .with_thinking_dialect(thinking_dialect)
        // Always explicit. Left unset, `ANTHROPIC_BASE_URL` would redirect this
        // provider — and the key with it — to a host the settings never named.
        .with_base_url(base_url.unwrap_or(horsie_llm_providers::anthropic::DEFAULT_BASE_URL));
    Ok(Arc::new(p))
}

fn build_openai(
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    thinking_dialect: ThinkingDialect,
    forced_tools_disable_thinking: bool,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key = required_key(provider, api_key)?;
    let p = OpenAiProvider::with_api_key(key)
        .map_err(|e| e.to_string())?
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_thinking_dialect(thinking_dialect)
        .with_forced_tools_disable_thinking(forced_tools_disable_thinking)
        .with_base_url(base_url.unwrap_or(horsie_llm_providers::openai::DEFAULT_BASE_URL));
    Ok(Arc::new(p))
}

fn build_responses(
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    thinking_dialect: ThinkingDialect,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key = required_key(provider, api_key)?;
    let p = ResponsesProvider::with_api_key(key)
        .map_err(|e| e.to_string())?
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_thinking_dialect(thinking_dialect)
        // Both OpenAI dialects now take a bare host: the client appends
        // `/v1/chat/completions` or `/v1/responses` itself.
        .with_base_url(base_url.unwrap_or(horsie_llm_providers::openai::DEFAULT_BASE_URL));
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
    /// Whose credential this is. A refreshed token must land back in the
    /// account it was refreshed for.
    user: UserId,
    provider: String,
}

#[async_trait]
impl TokenStore for DbTokenStore {
    async fn load(&self) -> Result<Option<StoredTokens>, ResponsesError> {
        read_one_provider_oauth(&self.db, &self.user, &self.provider)
            .await
            .map_err(|e| ResponsesError::Authentication(e.to_string()))
    }

    async fn save(&self, tokens: StoredTokens) -> Result<(), ResponsesError> {
        write_provider_oauth(&self.db, &self.user, &self.provider, &tokens)
            .await
            .map_err(|e| ResponsesError::Authentication(e.to_string()))
    }
}

/// The credential store one ChatGPT provider refreshes through.
pub(crate) fn token_store(db: &Db, user: &UserId, provider: &str) -> Arc<dyn TokenStore> {
    Arc::new(DbTokenStore {
        db: db.clone(),
        user: user.clone(),
        provider: provider.to_string(),
    })
}

/// Upsert one provider's credential.
pub(crate) async fn write_provider_oauth(
    db: &Db,
    user: &UserId,
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
        "INSERT INTO provider_oauth (user_id, provider, access, refresh, expires_at, account_id, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(user_id, provider) DO UPDATE SET access = excluded.access, \
         refresh = excluded.refresh, expires_at = excluded.expires_at, \
         account_id = excluded.account_id, updated_at = excluded.updated_at",
    ))
    .bind(user.as_str())
    .bind(provider)
    .bind(&tokens.access_token)
    .bind(&tokens.refresh_token)
    .bind(expires_at_column(tokens))
    .bind(tokens.account_id.clone().unwrap_or_default())
    .bind(now)
    .execute(db.pool())
    .await
    .map(|_| ())
}

// ── provider_oauth column mapping ────────────────────────────────────────────
//
// The table predates `async_llm::responses::chatgpt::StoredTokens`: it keeps its
// own column names, and its `expires_at`/`account_id` are `NOT NULL` where the
// struct has them optional. The `id_token` is deliberately not stored — the only
// thing ever read out of it is the account id, which has its own column, so
// persisting the raw JWT would be keeping a credential no one reads.

/// An absent expiry is stored as `0`, which reads back as "expired" and so
/// refreshes on first use — the safe direction for a `NOT NULL` column.
fn expires_at_column(tokens: &StoredTokens) -> i64 {
    tokens
        .expires_at
        .and_then(|expires_at| i64::try_from(expires_at).ok())
        .unwrap_or(0)
}

fn stored_tokens_from_row(row: &sqlx::any::AnyRow) -> Result<StoredTokens, sqlx::Error> {
    let account_id: String = row.try_get("account_id")?;
    let expires_at: i64 = row.try_get("expires_at")?;
    Ok(StoredTokens {
        access_token: row.try_get("access")?,
        refresh_token: row.try_get("refresh")?,
        id_token: None,
        account_id: Some(account_id).filter(|id| !id.is_empty()),
        expires_at: u64::try_from(expires_at).ok(),
    })
}

/// One provider's credential, as the refresh path re-reads it.
async fn read_one_provider_oauth(
    db: &Db,
    user: &UserId,
    provider: &str,
) -> Result<Option<StoredTokens>, sqlx::Error> {
    let sql = db.q(
        "SELECT access, refresh, expires_at, account_id FROM provider_oauth \
         WHERE user_id = ? AND provider = ?",
    );
    sqlx::query(&sql)
        .bind(user.as_str())
        .bind(provider)
        .fetch_optional(db.pool())
        .await?
        .as_ref()
        .map(stored_tokens_from_row)
        .transpose()
}

/// Build the live `ChatGptTokens` for every stored credential.
fn live_chatgpt_tokens(
    db: &Db,
    user: &UserId,
    stored: HashMap<String, StoredTokens>,
) -> HashMap<String, Arc<ChatGptTokens>> {
    stored
        .into_iter()
        .map(|(provider, tokens)| {
            let store = token_store(db, user, &provider);
            (
                provider,
                Arc::new(ChatGptTokens::new(
                    tokens,
                    store,
                    horsie_llm_providers::responses::chatgpt_auth(),
                )),
            )
        })
        .collect()
}

// ── secret + value helpers ───────────────────────────────────────────────────

/// Write-only secret input: `None` keeps the stored value, `Some("")` clears,
/// `Some(v)` sets.
/// Field validation for one provider, returning its trimmed name.
///
/// Every rule the whole-document update applied per entry, minus duplicate
/// detection: one request now carries one provider, so a repeated name is just
/// an upsert.
fn validate_provider(input: &ProviderInput) -> Result<String, String> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err("provider name cannot be empty".into());
    }
    if !matches!(
        input.kind.as_str(),
        "anthropic" | "openai" | "openai-responses" | "chatgpt"
    ) {
        return Err(format!(
            "unsupported provider kind '{}' (expected 'anthropic', 'openai', \
             'openai-responses' or 'chatgpt')",
            input.kind
        ));
    }
    Ok(name.to_string())
}

/// Field validation for one model, returning its trimmed alias. The provider it
/// names is checked by the registry rebuild, not here.
fn validate_model(input: &ModelInput) -> Result<String, String> {
    let alias = input.alias.trim();
    if alias.is_empty() {
        return Err("model alias cannot be empty".into());
    }
    if input.model_id.trim().is_empty() {
        return Err(format!("model '{alias}' needs a model id"));
    }
    if let Some(d) = input.thinking_dialect.as_deref()
        && ThinkingDialect::parse(d).is_none()
    {
        return Err(format!(
            "model '{alias}' has unknown thinking dialect '{d}'"
        ));
    }
    let offered: Vec<String> = input.thinking_efforts.clone().unwrap_or_default();
    for e in &offered {
        if ThinkingEffort::parse(e).is_none() {
            return Err(format!(
                "model '{alias}' offers unknown thinking effort '{e}'"
            ));
        }
    }
    if let Some(def) = input.thinking_effort.as_deref()
        && !offered.iter().any(|e| e == def)
    {
        return Err(format!(
            "model '{alias}' default thinking effort '{def}' is not among its offered efforts"
        ));
    }
    Ok(alias.to_string())
}

/// Drop a provider's stored ChatGPT sign-in, so the next provider to take that
/// name cannot inherit a live refresh token.
async fn delete_provider_oauth(
    db: &Db,
    tx: &mut sqlx::AnyConnection,
    user: &UserId,
    name: &str,
) -> Result<(), String> {
    sqlx::query(&db.q("DELETE FROM provider_oauth WHERE user_id = ? AND provider = ?"))
        .bind(user.as_str())
        .bind(name)
        .execute(tx)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

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

/// Map a vendor's announced capabilities to the settings-wire view.
///
/// Two wire types rather than one because they answer to different contracts:
/// the vendor protocol's is what a vendor announces about itself, and the
/// settings view's is what the UI renders. They agree today and are free to
/// diverge.
fn vendor_caps_view(
    caps: horsie_models::runtime_vendor::RuntimeVendorCapabilities,
) -> VendorCapabilities {
    VendorCapabilities {
        supports_provisioning: caps.supports_provisioning,
    }
}

fn provider_view(r: &ProviderRow, signed_in: bool) -> ProviderView {
    ProviderView {
        name: r.name.clone(),
        kind: r.kind.clone(),
        base_url: r.base_url.clone(),
        // Whichever credential this kind actually uses. A stored key on a
        // ChatGPT row would be a leftover the write path now clears, and it
        // never authorized anything even while it was there.
        has_credential: match r.kind.as_str() {
            "chatgpt" => signed_in,
            _ => r.api_key.as_deref().is_some_and(|s| !s.is_empty()),
        },
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
    user: &UserId,
) -> Result<HashMap<String, StoredTokens>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q(
        "SELECT provider, access, refresh, expires_at, account_id FROM provider_oauth \
         WHERE user_id = ?",
    );
    let rows = sqlx::query(&sql).bind(user.as_str()).fetch_all(ex).await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        out.insert(
            r.try_get::<String, _>("provider")?,
            stored_tokens_from_row(r)?,
        );
    }
    Ok(out)
}

async fn read_providers<'e, E>(
    db: &Db,
    ex: E,
    user: &UserId,
) -> Result<Vec<ProviderRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q(
        "SELECT name, kind, base_url, api_key, keep_thinking_signature FROM providers \
         WHERE user_id = ? ORDER BY name",
    );
    let rows = sqlx::query(&sql).bind(user.as_str()).fetch_all(ex).await?;
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

async fn read_models<'e, E>(db: &Db, ex: E, user: &UserId) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q(
        "SELECT alias, provider, model_id, max_tokens, context_window, thinking_efforts, thinking_effort, thinking_dialect, forced_tools_disable_thinking FROM models WHERE user_id = ? ORDER BY alias",
    );
    let rows = sqlx::query(&sql).bind(user.as_str()).fetch_all(ex).await?;
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

/// This account's dial secret, creating it on first use.
///
/// `begin_write`, not `begin`: this reads the setting and then writes it when
/// absent, and a deferred transaction that upgrades to a write that late loses
/// to any writer that committed in between — SQLite answers `database is
/// locked` and no busy timeout retries it.
async fn load_or_create_dial_secret(db: &Db, user: &UserId) -> Result<Arc<Vec<u8>>, String> {
    let mut tx = db.begin_write().await.map_err(|e| e.to_string())?;
    if let Some(existing) = read_setting(db, &mut *tx, user, RUNTIME_DIAL_SECRET_KEY)
        .await
        .map_err(|e| e.to_string())?
        && let Ok(bytes) = hex::decode(&existing)
        && !bytes.is_empty()
    {
        return Ok(Arc::new(bytes));
    }
    let mut secret = vec![0u8; 32];
    rand::fill(&mut secret[..]);
    let sql = db.q(
        "INSERT INTO settings (user_id, key, value) VALUES (?, ?, ?) \
         ON CONFLICT(user_id, key) DO UPDATE SET value = excluded.value",
    );
    sqlx::query(&sql)
        .bind(user.as_str())
        .bind(RUNTIME_DIAL_SECRET_KEY)
        .bind(hex::encode(&secret))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Arc::new(secret))
}

/// Settings key holding this account's hex-encoded dial secret.
const RUNTIME_DIAL_SECRET_KEY: &str = "runtime_dial_secret";

async fn read_setting<'e, E>(
    db: &Db,
    ex: E,
    user: &UserId,
    key: &str,
) -> Result<Option<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let sql = db.q("SELECT value FROM settings WHERE user_id = ? AND key = ?");
    let row = sqlx::query(&sql)
        .bind(user.as_str())
        .bind(key)
        .fetch_optional(ex)
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

    // The migration-version uniqueness check that used to live here now covers
    // both dialect directories, in `crate::db::tests`.
    async fn open() -> OpenedConfig {
        DbConfigStore::open_on(
            crate::db::testing::db().await,
            StoreDeps { info: info() },
            UserId::new("1"),
        )
        .await
        .unwrap()
    }

    /// The flag has to survive the save→read→build round trip, because it is
    /// what keeps a forced-handoff agent from 400ing on DeepSeek.
    #[tokio::test]
    async fn forced_tools_flag_persists_through_a_settings_update() {
        let store = open().await.store;

        store
            .seed(
                vec![ProviderInput {
                    name: "deepseek".into(),
                    kind: "openai".into(),
                    base_url: Some("https://api.deepseek.com".into()),
                    api_key: Some("k".into()),
                    keep_thinking_signature: None,
                }],
                vec![ModelInput {
                    alias: "ds".into(),
                    provider: "deepseek".into(),
                    model_id: "deepseek-v4-flash".into(),
                    max_tokens: Some(393_216),
                    context_window: None,
                    thinking_efforts: Some(vec!["none".into(), "high".into()]),
                    thinking_effort: Some("high".into()),
                    thinking_dialect: Some("openai_effort".into()),
                    forced_tools_disable_thinking: Some(true),
                }],
            )
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
            .seed(
                vec![provider("p", Some("sk-inline"))],
                vec![model("m", "p")],
            )
            .await
            .expect("update ok");
        assert_eq!(view.models.len(), 1);
        assert!(view.providers[0].has_credential);
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
            .seed(
                vec![provider_of_kind("p", "openai-responses", Some("sk-inline"))],
                vec![model("m", "p")],
            )
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
            .seed(
                vec![provider_of_kind("p", "chatgpt", None)],
                vec![model("m", "p")],
            )
            .await
            .expect_err("no credential yet");

        assert!(err.contains("sign in"), "unhelpful error: {err}");
    }

    #[tokio::test]
    async fn a_chatgpt_provider_builds_once_a_credential_is_stored() {
        let o = open().await;

        // A provider with no models: accepted, since nothing needs building yet.
        o.store
            .seed(vec![provider_of_kind("p", "chatgpt", None)], vec![])
            .await
            .expect("provider alone is fine");

        write_provider_oauth(
            &o.db,
            &UserId::new("1"),
            "p",
            &StoredTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                account_id: Some("acct_1".into()),
                expires_at: Some(9_999_999_999),
            },
        )
        .await
        .expect("stored");

        o.store
            .seed(
                vec![provider_of_kind("p", "chatgpt", None)],
                vec![model("m", "p")],
            )
            .await
            .expect("update ok");

        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    /// The provider crates all read a key out of the process environment when
    /// none is passed. A blank provider row must never reach that fallback: it
    /// would spend a credential the operator never attached to this provider.
    #[tokio::test]
    async fn a_provider_without_a_key_is_rejected_rather_than_reading_the_environment() {
        // SAFETY: single-threaded test process section; the value is removed
        // again before any other provider test could observe it.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-from-the-environment") };
        let o = open().await;

        let err = o
            .store
            .seed(vec![provider("p", None)], vec![model("m", "p")])
            .await
            .expect_err("a key-less provider must not build");
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

        assert!(err.contains("no API key"), "unhelpful error: {err}");
        assert!(err.contains('p'), "the error names the provider: {err}");
    }

    /// Both directions of a kind change. A credential belongs to the kind that
    /// uses it, and neither one may outlive the switch: the leftover key made
    /// the settings list report "Key set" for a provider authorized by OAuth,
    /// and a leftover token would be inherited by whatever the name becomes.
    #[tokio::test]
    async fn a_kind_change_drops_the_credential_the_new_kind_cannot_use() {
        let o = open().await;
        o.store
            .seed(vec![provider("p", Some("sk-inline"))], vec![])
            .await
            .expect("update ok");

        // anthropic → chatgpt: the API key goes.
        let view = o
            .store
            .seed(vec![provider_of_kind("p", "chatgpt", None)], vec![])
            .await
            .expect("update ok");
        assert!(
            !view.providers[0].has_credential,
            "a stale API key must not read as a ChatGPT sign-in"
        );

        write_provider_oauth(
            &o.db,
            &UserId::new("1"),
            "p",
            &StoredTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                account_id: Some("acct_1".into()),
                expires_at: Some(9_999_999_999),
            },
        )
        .await
        .expect("stored");
        assert!(o.store.view().await.unwrap().providers[0].has_credential);

        // chatgpt → anthropic: the sign-in goes, and the key it once had does
        // not come back with it.
        let view = o
            .store
            .seed(vec![provider("p", None)], vec![])
            .await
            .expect("update ok");
        assert!(!view.providers[0].has_credential);
        assert!(
            read_provider_oauth(&o.db, o.db.pool(), &UserId::new("1"))
                .await
                .unwrap()
                .is_empty(),
            "a provider that is no longer a ChatGPT plan kept its refresh token"
        );
    }

    /// A credential must not outlive the provider it belongs to: a later
    /// provider reusing the name would otherwise silently inherit a live
    /// refresh token.
    #[tokio::test]
    async fn removing_a_provider_drops_its_sign_in() {
        let o = open().await;
        o.store
            .seed(vec![provider_of_kind("p", "chatgpt", None)], vec![])
            .await
            .expect("update ok");
        write_provider_oauth(
            &o.db,
            &UserId::new("1"),
            "p",
            &StoredTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                account_id: Some("acct_1".into()),
                expires_at: Some(9_999_999_999),
            },
        )
        .await
        .expect("stored");

        o.store
            .delete_provider("p")
            .await
            .expect("provider deleted");

        let left = read_provider_oauth(&o.db, o.db.pool(), &UserId::new("1"))
            .await
            .unwrap();
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
            .seed(vec![provider_of_kind("p", "nonsense", None)], vec![])
            .await
            .expect_err("unknown kind");

        assert!(err.contains("unsupported provider kind"), "got: {err}");
    }

    #[tokio::test]
    async fn context_window_defaults_for_known_models_and_stays_editable() {
        let o = open().await;
        let view = o
            .store
            .seed(
                vec![provider("p", Some("sk-inline"))],
                vec![
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
                ],
            )
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
            .seed(vec![provider("p", Some("sk-secret"))], vec![])
            .await
            .unwrap();
        // Re-send without a key → keep it (the view still reports a stored key).
        let view = o
            .store
            .seed(vec![provider("p", None)], vec![])
            .await
            .unwrap();
        assert!(view.providers[0].has_credential);
    }

    #[tokio::test]
    async fn update_rejects_unknown_provider_and_rolls_back() {
        let o = open().await;
        o.store
            .seed(vec![provider("p", Some("k"))], vec![model("m", "p")])
            .await
            .unwrap();
        let err = o
            .store
            .seed(vec![], vec![model("m", "ghost")])
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
            .seed(
                vec![provider_kind("local", "openai")],
                vec![model("m", "local")],
            )
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
            .seed(vec![provider_kind("bogus", "cohere")], vec![])
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

        let flag = |view: &SettingsView, name: &str| {
            view.providers
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("no provider {name}"))
                .keep_thinking_signature
        };

        // Defaults off for a fresh provider.
        let view = o
            .store
            .seed(
                vec![provider("kimi", Some("sk-test"))],
                vec![model("m", "kimi")],
            )
            .await
            .expect("update succeeds");
        assert!(!flag(&view, "kimi"));

        // Opting in persists and reads back. Providers accumulate now rather
        // than being replaced wholesale, so this is looked up by name.
        let mut p = provider("real-anthropic", Some("sk-test"));
        p.keep_thinking_signature = Some(true);
        let view = o
            .store
            .seed(vec![p], vec![model("m2", "real-anthropic")])
            .await
            .expect("update succeeds");
        assert!(flag(&view, "real-anthropic"));
        assert!(!flag(&view, "kimi"), "the other provider is untouched");
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
            .seed(vec![provider("p", Some("sk-test"))], vec![m])
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
            .seed(vec![provider("p", Some("sk-test"))], vec![model("m", "p")])
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
            .seed(vec![provider("p", Some("sk-test"))], vec![m])
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
            .seed(vec![provider("p", Some("sk-test"))], vec![m])
            .await
            .expect_err("unknown dialect must be rejected");
        assert!(err.contains("telepathy"), "error should name it: {err}");
    }

    // The WAL/synchronous pragmas moved to `Db::open`'s `after_connect` hook,
    // and so did the test that guards them: `crate::db::tests`.

    // --- per-resource operations ---

    #[tokio::test]
    async fn upsert_provider_creates_then_updates_one_row() {
        let o = open().await;
        o.store
            .upsert_provider(provider("a", Some("sk-a")))
            .await
            .expect("create a");
        o.store
            .upsert_provider(provider("b", Some("sk-b")))
            .await
            .expect("create b");

        // The point of the whole change: touching one provider leaves the other
        // alone. Under `PUT /api/config` the second write would drop `a`.
        let view = o.store.view().await.unwrap();
        assert_eq!(view.providers.len(), 2);

        let updated = o
            .store
            .upsert_provider(provider("a", Some("sk-a2")))
            .await
            .expect("update a");
        assert!(updated.has_credential);
        assert_eq!(o.store.view().await.unwrap().providers.len(), 2);
    }

    #[tokio::test]
    async fn upsert_provider_keeps_stored_key_when_omitted_and_clears_on_empty() {
        let o = open().await;
        o.store
            .upsert_provider(provider("p", Some("sk-keep")))
            .await
            .unwrap();

        let mut omitted = provider("p", None);
        omitted.api_key = None;
        assert!(
            o.store
                .upsert_provider(omitted)
                .await
                .unwrap()
                .has_credential,
            "omitted api_key must keep the stored key"
        );

        let mut cleared = provider("p", None);
        cleared.api_key = Some(String::new());
        assert!(
            !o.store
                .upsert_provider(cleared)
                .await
                .unwrap()
                .has_credential,
            "empty api_key must clear the stored key"
        );
    }

    #[tokio::test]
    async fn upsert_provider_rejects_unknown_kind() {
        let o = open().await;
        let err = o
            .store
            .upsert_provider(provider_of_kind("p", "gemini", Some("k")))
            .await
            .expect_err("unknown kind rejected");
        assert!(err.contains("unsupported provider kind"), "{err}");
    }

    #[tokio::test]
    async fn upsert_model_rejects_unknown_provider_and_rolls_back() {
        let o = open().await;
        let err = o
            .store
            .upsert_model(model("m", "nope"))
            .await
            .expect_err("unknown provider rejected");
        assert!(!err.is_empty());
        assert!(
            o.store.view().await.unwrap().models.is_empty(),
            "a rejected model must not persist"
        );
    }

    #[tokio::test]
    async fn delete_provider_is_blocked_while_a_model_references_it() {
        let o = open().await;
        o.store
            .upsert_provider(provider("p", Some("sk")))
            .await
            .unwrap();
        o.store.upsert_model(model("m", "p")).await.unwrap();

        let err = o
            .store
            .delete_provider("p")
            .await
            .expect_err("referenced provider is held open");
        assert!(err.contains("still used by model"), "{err}");
        assert!(err.contains('m'), "the error names the model: {err}");

        o.store.delete_model("m").await.expect("model deleted");
        o.store.delete_provider("p").await.expect("now deletable");
        assert!(o.store.view().await.unwrap().providers.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_missing_row_is_an_error() {
        let o = open().await;
        assert!(o.store.delete_provider("ghost").await.is_err());
        assert!(o.store.delete_model("ghost").await.is_err());
    }

    #[tokio::test]
    async fn delete_model_swaps_it_out_of_the_registry() {
        let o = open().await;
        o.store
            .upsert_provider(provider("p", Some("sk")))
            .await
            .unwrap();
        o.store.upsert_model(model("m", "p")).await.unwrap();
        assert!(o.registry.read().unwrap().contains_key("m"));

        o.store.delete_model("m").await.unwrap();
        assert!(
            !o.registry.read().unwrap().contains_key("m"),
            "a deleted model must leave the live registry too"
        );
    }

    #[tokio::test]
    async fn upsert_and_delete_drop_a_stale_chatgpt_sign_in() {
        // The rule wholesale rewrite got for free: a sign-in must not outlive
        // the kind that created it, or the next provider of that name silently
        // inherits a live refresh token.
        let o = open().await;
        o.store
            .upsert_provider(provider_of_kind("p", "chatgpt", None))
            .await
            .unwrap();
        write_provider_oauth(
            &o.db,
            &UserId::new("1"),
            "p",
            &StoredTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                account_id: Some("acct_1".into()),
                expires_at: Some(9_999_999_999),
            },
        )
        .await
        .expect("sign-in written");
        assert!(
            o.store.view().await.unwrap().providers[0].has_credential,
            "chatgpt provider is signed in"
        );

        // Switching it to a key-based kind must drop the sign-in.
        o.store
            .upsert_provider(provider_of_kind("p", "openai", None))
            .await
            .unwrap();
        assert!(
            !o.store.view().await.unwrap().providers[0].has_credential,
            "changing kind away from chatgpt must clear the sign-in"
        );
    }
}
