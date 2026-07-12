//! SQLite-backed [`ConfigStore`]. Owns the settings database, builds the live
//! provider registry and the runtime vendors from it, and applies edits:
//! provider/model/default-vendor changes swap the live registry (next turn sees
//! them); vendor changes persist and activate on the next restart.
//!
//! Vendors are generic — a `vendors(name, kind, config)` table plus a
//! kind-tagged config union — so a new vendor kind is a new match arm, not a
//! schema change. `postgres` is a future driver swap behind the same code.

use crate::config::ConfigStore;
use crate::sessions::spec::SharedProviderRegistry;
use crate::velos::VelosClient;
use crate::vendor::{LocalProcessVendor, RuntimeVendor, VelosVendor, VelosVendorSettings};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Secret};
use horsie_anthropic::AnthropicProvider;
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VelosView,
    VendorConfigInput, VendorConfigView, VendorView,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

type Registry = HashMap<String, Arc<dyn LlmProvider>>;

/// Deployment inputs the host supplies when opening the store.
pub struct StoreDeps {
    /// `horsie-runtime` binary the built-in `local` vendor spawns.
    pub runtime_bin: PathBuf,
    /// Root under which the built-in `local` vendor allocates managed
    /// workspaces (`<workspace_root>/<runtime_id>/<name>`).
    pub workspace_root: PathBuf,
    /// Read-only deployment paths, surfaced in the settings view.
    pub info: ServerInfo,
    /// Server HTTP base a co-located `local`-vendor runtime fetches plugin
    /// artifacts from (loopback, e.g. `http://127.0.0.1:3789`). `None` disables
    /// local-vendor plugin provisioning.
    pub public_http_base: Option<String>,
}

/// What [`DbConfigStore::open`] hands back: the store (for the HTTP layer) plus
/// the runtime objects the session supervisor needs.
pub struct OpenedConfig {
    pub store: Arc<DbConfigStore>,
    pub registry: SharedProviderRegistry,
    pub vendors: HashMap<String, Arc<dyn RuntimeVendor>>,
    /// The migrated connection pool, shared with feature stores (e.g. GitHub)
    /// that persist into the same settings DB.
    pub pool: SqlitePool,
}

pub struct DbConfigStore {
    pool: SqlitePool,
    registry: SharedProviderRegistry,
    default_vendor: RwLock<String>,
    /// Vendor names loaded this process (`local` + each vendor built at open).
    active_vendors: Vec<String>,
    /// Set once a vendor is edited — those changes need a restart, so the view
    /// reports `restart_required` until then.
    vendors_dirty: AtomicBool,
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

        let vendor_rows = read_vendors(&pool).await.map_err(|e| e.to_string())?;
        let vendors = build_vendors(
            &vendor_rows,
            deps.runtime_bin,
            deps.workspace_root,
            deps.public_http_base,
        )
        .await;
        let mut active_vendors: Vec<String> = vendors.keys().cloned().collect();
        active_vendors.sort();

        let default_vendor = read_setting(&pool, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());
        let default_vendor = if vendors.contains_key(&default_vendor) {
            default_vendor
        } else {
            eprintln!("warning: default vendor '{default_vendor}' is not loaded; using 'local'");
            "local".into()
        };

        let store = Arc::new(Self {
            pool: pool.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            active_vendors,
            vendors_dirty: AtomicBool::new(false),
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
        let vendor_rows = read_vendors(&self.pool).await.map_err(|e| e.to_string())?;
        let default_vendor = self.default_vendor();
        Ok(SettingsView {
            providers: provs.iter().map(provider_view).collect(),
            models: mods.iter().map(model_view).collect(),
            vendors: self.vendors_view(&default_vendor, &vendor_rows),
            default_vendor,
            info: self.info.clone(),
            restart_required: self.vendors_dirty.load(Ordering::Relaxed),
        })
    }

    fn vendors_view(&self, default_vendor: &str, rows: &[VendorRow]) -> Vec<VendorView> {
        let active = |name: &str| self.active_vendors.iter().any(|n| n == name);
        let mut out = vec![VendorView {
            name: "local".into(),
            active: active("local"),
            is_default: default_vendor == "local",
            config: None,
        }];
        for r in rows {
            let config = match r.kind.as_str() {
                "velos" => serde_json::from_str::<VelosConfig>(&r.config)
                    .ok()
                    .map(|vc| VendorConfigView::Velos(velos_view(&vc))),
                _ => None,
            };
            out.push(VendorView {
                name: r.name.clone(),
                active: active(&r.name),
                is_default: default_vendor == r.name,
                config,
            });
        }
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
                if p.kind != "anthropic" {
                    return Err(format!(
                        "unsupported provider kind '{}' (only 'anthropic')",
                        p.kind
                    ));
                }
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate provider '{name}'"));
                }
                let api_key = resolve_secret(&p.api_key, keep.get(name).copied());
                sqlx::query(
                    "INSERT INTO providers (name, kind, base_url, api_key_env, api_key) \
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(name)
                .bind(&p.kind)
                .bind(trimmed(&p.base_url))
                .bind(trimmed(&p.api_key_env))
                .bind(api_key)
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
                sqlx::query(
                    "INSERT INTO models (alias, provider, model_id, max_tokens) VALUES (?, ?, ?, ?)",
                )
                .bind(alias)
                .bind(&m.provider)
                .bind(m.model_id.trim())
                .bind(m.max_tokens.map(i64::from))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
        }

        if let Some(vendors) = &update.vendors {
            let existing = read_vendors(&mut *tx).await.map_err(|e| e.to_string())?;
            let keep: HashMap<&str, &str> = existing
                .iter()
                .map(|r| (r.name.as_str(), r.config.as_str()))
                .collect();
            let mut seen = HashSet::new();
            sqlx::query("DELETE FROM vendors")
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            for v in vendors {
                let name = v.name.trim();
                if name.is_empty() {
                    return Err("vendor name cannot be empty".into());
                }
                if name == "local" {
                    return Err("'local' is reserved and cannot be a configured vendor".into());
                }
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate vendor '{name}'"));
                }
                let (kind, config) = build_vendor_config(name, &v.config, keep.get(name).copied())?;
                sqlx::query("INSERT INTO vendors (name, kind, config) VALUES (?, ?, ?)")
                    .bind(name)
                    .bind(kind)
                    .bind(config)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }

        if let Some(dv) = &update.default_vendor {
            if !self.active_vendors.iter().any(|n| n == dv) {
                return Err(format!(
                    "vendor '{dv}' is not loaded (available: {})",
                    self.active_vendors.join(", ")
                ));
            }
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
        if update.vendors.is_some() {
            self.vendors_dirty.store(true, Ordering::Relaxed);
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
    api_key_env: Option<String>,
    api_key: Option<String>,
}

struct ModelRow {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<i64>,
}

struct VendorRow {
    name: String,
    kind: String,
    config: String,
}

/// Server-side velos config, deserialized from a vendor row's JSON. Defaults
/// mirror the documented file config; `token` deserializes transparently into a
/// redacting [`Secret`].
#[derive(Deserialize)]
struct VelosConfig {
    server_url: String,
    image: String,
    advertise_host: String,
    #[serde(default)]
    token: Option<Secret>,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default = "default_runtime_bin")]
    runtime_bin: String,
    #[serde(default = "default_workspace_root")]
    workspace_root: String,
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_cpu")]
    cpu: u32,
    #[serde(default = "default_memory_mib")]
    memory_mib: u64,
    #[serde(default = "default_connect_timeout_secs")]
    connect_timeout_secs: u64,
    #[serde(default)]
    http_port: Option<u32>,
}

fn default_runtime_bin() -> String {
    "horsie-runtime".into()
}
fn default_workspace_root() -> String {
    "/workspace".into()
}
fn default_listen() -> String {
    "0.0.0.0:0".into()
}
fn default_cpu() -> u32 {
    2
}
fn default_memory_mib() -> u64 {
    1024
}
fn default_connect_timeout_secs() -> u64 {
    60
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
        if p.kind != "anthropic" {
            return Err(format!(
                "provider '{}' has unsupported kind '{}'",
                p.name, p.kind
            ));
        }
        let max_tokens = m.max_tokens.and_then(|v| u32::try_from(v).ok());
        reg.insert(
            m.alias.clone(),
            build_anthropic(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                p.api_key_env.as_deref(),
                &m.model_id,
                max_tokens,
            )?,
        );
    }
    Ok(reg)
}

fn build_anthropic(
    base_url: Option<&str>,
    api_key: Option<&str>,
    api_key_env: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key: Option<Secret> = match (api_key, api_key_env) {
        (Some(k), _) if !k.is_empty() => Some(Secret::from(k)),
        (Some(_), _) => return Err("inline api_key is empty".into()),
        (None, Some(var)) => {
            let v = std::env::var(var)
                .map_err(|_| format!("env var '{var}' for provider is not set"))?;
            if v.is_empty() {
                return Err(format!("env var '{var}' for provider is empty"));
            }
            Some(Secret::from(v))
        }
        (None, None) => None,
    };
    let mut p = match key {
        Some(k) => AnthropicProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => AnthropicProvider::new().map_err(|e| e.to_string())?,
    };
    p = p.with_model(model_id).with_max_tokens(max_tokens);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

/// Build the vendor set: `local` always, plus one per configured row. A vendor
/// that fails to start is logged and left out (reported inactive), never fatal.
async fn build_vendors(
    rows: &[VendorRow],
    runtime_bin: PathBuf,
    workspace_root: PathBuf,
    public_http_base: Option<String>,
) -> HashMap<String, Arc<dyn RuntimeVendor>> {
    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    vendors.insert(
        "local".into(),
        Arc::new(LocalProcessVendor::new(
            runtime_bin,
            workspace_root,
            public_http_base,
        )),
    );
    for r in rows {
        match r.kind.as_str() {
            "velos" => match serde_json::from_str::<VelosConfig>(&r.config) {
                Ok(vc) => match build_velos_vendor(&vc).await {
                    Ok(v) => {
                        println!(
                            "velos vendor '{}' enabled (server {})",
                            r.name, vc.server_url
                        );
                        vendors.insert(r.name.clone(), Arc::new(v));
                    }
                    Err(e) => eprintln!("warning: velos vendor '{}' failed to start ({e})", r.name),
                },
                Err(e) => eprintln!(
                    "warning: velos vendor '{}' has invalid config ({e})",
                    r.name
                ),
            },
            other => eprintln!("warning: vendor '{}' has unknown kind '{other}'", r.name),
        }
    }
    vendors
}

async fn build_velos_vendor(vc: &VelosConfig) -> Result<VelosVendor, String> {
    let token = resolve_velos_token(vc)?;
    let client =
        VelosClient::new(&vc.server_url, token).map_err(|e| format!("velos client: {e}"))?;
    let listen: SocketAddr = vc
        .listen
        .parse()
        .map_err(|e| format!("invalid velos listen '{}': {e}", vc.listen))?;
    let settings = VelosVendorSettings {
        image: vc.image.clone(),
        runtime_bin: vc.runtime_bin.clone(),
        workspace_root: vc.workspace_root.clone(),
        advertise_host: vc.advertise_host.clone(),
        listen,
        cpu: vc.cpu,
        memory_bytes: vc.memory_mib.saturating_mul(1024 * 1024),
        connect_timeout: Duration::from_secs(vc.connect_timeout_secs),
        http_port: vc.http_port,
    };
    VelosVendor::bind(Arc::new(client), settings)
        .await
        .map_err(|e| format!("velos vendor: {e}"))
}

fn resolve_velos_token(vc: &VelosConfig) -> Result<Option<Secret>, String> {
    match (&vc.token, &vc.token_env) {
        (Some(t), _) => {
            if t.is_empty() {
                return Err("velos inline token is empty".into());
            }
            Ok(Some(t.clone()))
        }
        (None, Some(var)) => {
            let v = std::env::var(var)
                .map_err(|_| format!("velos token env var '{var}' is not set"))?;
            if v.is_empty() {
                return Err(format!("velos token env var '{var}' is empty"));
            }
            Ok(Some(Secret::from(v)))
        }
        (None, None) => Ok(None),
    }
}

/// Turn a vendor config input into a `(kind, config-json)` row, carrying a
/// stored secret forward when the input omits it.
fn build_vendor_config(
    name: &str,
    input: &VendorConfigInput,
    existing: Option<&str>,
) -> Result<(&'static str, String), String> {
    match input {
        VendorConfigInput::Velos(v) => {
            if v.server_url.trim().is_empty()
                || v.image.trim().is_empty()
                || v.advertise_host.trim().is_empty()
            {
                return Err(format!(
                    "velos vendor '{name}' needs a server URL, image, and advertise host"
                ));
            }
            if let Some(listen) = trimmed(&v.listen)
                && listen.parse::<SocketAddr>().is_err()
            {
                return Err(format!(
                    "velos vendor '{name}' has an invalid listen '{listen}'"
                ));
            }
            let existing_token = existing
                .and_then(|c| serde_json::from_str::<Value>(c).ok())
                .and_then(|val| val.get("token").and_then(Value::as_str).map(String::from));
            let token = resolve_secret(&v.token, existing_token.as_deref());

            let mut m = Map::new();
            m.insert("server_url".into(), json!(v.server_url.trim()));
            m.insert("image".into(), json!(v.image.trim()));
            m.insert("advertise_host".into(), json!(v.advertise_host.trim()));
            if let Some(t) = token {
                m.insert("token".into(), json!(t));
            }
            insert_trimmed(&mut m, "token_env", &v.token_env);
            insert_trimmed(&mut m, "runtime_bin", &v.runtime_bin);
            insert_trimmed(&mut m, "workspace_root", &v.workspace_root);
            insert_trimmed(&mut m, "listen", &v.listen);
            if let Some(x) = v.cpu {
                m.insert("cpu".into(), json!(x));
            }
            if let Some(x) = v.memory_mib {
                m.insert("memory_mib".into(), json!(x));
            }
            if let Some(x) = v.connect_timeout_secs {
                m.insert("connect_timeout_secs".into(), json!(x));
            }
            if let Some(x) = v.http_port {
                m.insert("http_port".into(), json!(x));
            }
            let config = serde_json::to_string(&Value::Object(m)).map_err(|e| e.to_string())?;
            Ok(("velos", config))
        }
    }
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

fn insert_trimmed(m: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(s) = trimmed(v) {
        m.insert(key.to_string(), json!(s));
    }
}

// ── projections ──────────────────────────────────────────────────────────────

fn provider_view(r: &ProviderRow) -> ProviderView {
    ProviderView {
        name: r.name.clone(),
        kind: r.kind.clone(),
        base_url: r.base_url.clone(),
        api_key_env: r.api_key_env.clone(),
        has_inline_key: r.api_key.as_deref().is_some_and(|s| !s.is_empty()),
    }
}

fn model_view(r: &ModelRow) -> ModelView {
    ModelView {
        alias: r.alias.clone(),
        provider: r.provider.clone(),
        model_id: r.model_id.clone(),
        max_tokens: r.max_tokens.and_then(|v| u32::try_from(v).ok()),
    }
}

fn velos_view(vc: &VelosConfig) -> VelosView {
    VelosView {
        server_url: vc.server_url.clone(),
        image: vc.image.clone(),
        advertise_host: vc.advertise_host.clone(),
        token_env: vc.token_env.clone(),
        has_inline_token: vc.token.as_ref().is_some_and(|t| !t.is_empty()),
        runtime_bin: vc.runtime_bin.clone(),
        workspace_root: vc.workspace_root.clone(),
        listen: vc.listen.clone(),
        cpu: vc.cpu,
        memory_mib: vc.memory_mib,
        connect_timeout_secs: vc.connect_timeout_secs,
        http_port: vc.http_port,
    }
}

// ── connection + row reads ───────────────────────────────────────────────────

async fn open_pool(url: &str) -> Result<SqlitePool, String> {
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
        "SELECT name, kind, base_url, api_key_env, api_key FROM providers ORDER BY name",
    )
    .fetch_all(ex)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ProviderRow {
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            base_url: r.try_get("base_url")?,
            api_key_env: r.try_get("api_key_env")?,
            api_key: r.try_get("api_key")?,
        });
    }
    Ok(out)
}

async fn read_models<'e, E>(ex: E) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows =
        sqlx::query("SELECT alias, provider, model_id, max_tokens FROM models ORDER BY alias")
            .fetch_all(ex)
            .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ModelRow {
            alias: r.try_get("alias")?,
            provider: r.try_get("provider")?,
            model_id: r.try_get("model_id")?,
            max_tokens: r.try_get("max_tokens")?,
        });
    }
    Ok(out)
}

async fn read_vendors<'e, E>(ex: E) -> Result<Vec<VendorRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query("SELECT name, kind, config FROM vendors ORDER BY name")
        .fetch_all(ex)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(VendorRow {
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            config: r.try_get("config")?,
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
    use horsie_models::settings::{ModelInput, ProviderInput, VelosInput, VendorInput};

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
        DbConfigStore::open(
            &format!("sqlite://{}/t.db", dir.display()),
            StoreDeps {
                runtime_bin: PathBuf::from("horsie-runtime"),
                workspace_root: dir.join("workspaces"),
                info: info(),
                public_http_base: None,
            },
        )
        .await
        .unwrap()
    }

    fn provider(name: &str, key: Option<&str>) -> ProviderInput {
        ProviderInput {
            name: name.into(),
            kind: "anthropic".into(),
            base_url: Some("http://localhost:1".into()),
            api_key_env: None,
            api_key: key.map(str::to_string),
        }
    }

    fn model(alias: &str, provider: &str) -> ModelInput {
        ModelInput {
            alias: alias.into(),
            provider: provider.into(),
            model_id: "id".into(),
            max_tokens: None,
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
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update ok");
        assert_eq!(view.models.len(), 1);
        assert!(view.providers[0].has_inline_key);
        assert!(o.registry.read().unwrap().contains_key("m"));
    }

    #[tokio::test]
    async fn update_preserves_inline_key_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-secret"))]),
                models: None,
                vendors: None,
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
                vendors: None,
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
                vendors: None,
                default_vendor: None,
            })
            .await
            .unwrap();
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![]),
                models: Some(vec![model("m", "ghost")]),
                vendors: None,
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

    #[tokio::test]
    async fn velos_vendor_persists_redacted_and_flags_restart() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![VendorInput {
                    name: "cluster-a".into(),
                    config: VendorConfigInput::Velos(VelosInput {
                        server_url: "http://velos:8080".into(),
                        image: "img".into(),
                        advertise_host: "10.0.0.5".into(),
                        token: Some("secret".into()),
                        token_env: None,
                        runtime_bin: None,
                        workspace_root: None,
                        listen: None,
                        cpu: None,
                        memory_mib: None,
                        connect_timeout_secs: None,
                        http_port: None,
                    }),
                }]),
                default_vendor: None,
            })
            .await
            .expect("velos update ok");
        assert!(view.restart_required);
        let v = view
            .vendors
            .iter()
            .find(|v| v.name == "cluster-a")
            .expect("vendor present");
        assert!(!v.active); // built at startup only
        match &v.config {
            Some(VendorConfigView::Velos(velos)) => {
                assert!(velos.has_inline_token);
                assert_eq!(velos.runtime_bin, "horsie-runtime"); // default applied
            }
            None => panic!("expected velos config"),
        }
    }

    #[tokio::test]
    async fn update_rejects_unknown_default_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let err = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: None,
                default_vendor: Some("cluster-a".into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("cluster-a"));
        assert_eq!(o.store.default_vendor(), "local");
    }
}
