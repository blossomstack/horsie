//! SQLite-backed [`ConfigStore`]. Owns the settings database, builds the live
//! provider registry and the runtime vendors from it, and applies edits:
//! provider/model/default-vendor changes swap the live registry (next turn
//! sees them); vendor changes reconcile the live vendor map immediately in
//! the common case (new, previously-inactive, or an active vendor's
//! non-listener settings) — only a change to an already-active vendor's
//! `listen`/`advertise_host`/`server_url` still needs a restart.
//!
//! Vendors are generic — a `vendors(name, kind, config)` table plus a
//! kind-tagged config union — so a new vendor kind is a new match arm, not a
//! schema change. `postgres` is a future driver swap behind the same code.

use crate::config::ConfigStore;
use crate::sessions::spec::{SharedProviderRegistry, SharedVendors};
use crate::velos::{VelosClient, VelosError};
use crate::vendor::{
    LocalDaemonRegistry, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Secret};
use horsie_anthropic::AnthropicProvider;
use horsie_models::settings::{
    ModelView, ProviderView, ServerInfo, SettingsUpdate, SettingsView, VelosView,
    VendorConfigInput, VendorConfigView, VendorTestResult, VendorView,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

type Registry = HashMap<String, Arc<dyn LlmProvider>>;

/// Deployment inputs the host supplies when opening the store.
pub struct StoreDeps {
    /// Read-only deployment paths, surfaced in the settings view.
    pub info: ServerInfo,
    /// Address the shared local-runtime-vendor listener binds. User-launched
    /// `horsie-runtime --endpoint ws://...` daemons dial back here. `None`
    /// disables the shared local vendor entirely (no listener bound, no
    /// `"local"` vendor kind ever registered).
    pub local_runtime_listen: Option<String>,
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
    default_vendor: RwLock<String>,
    /// Live runtime vendors, kept in sync with the DB by `update()`'s
    /// reconciliation so most vendor edits apply without a restart.
    vendors: SharedVendors,
    /// Concrete handles for vendor kinds that support live reconfigure
    /// (currently only `velos`), keyed by name — lets `update()` call
    /// `.reconfigure()` on the right concrete type without downcasting the
    /// generic `vendors` map.
    velos_instances: RwLock<HashMap<String, Arc<VelosVendor>>>,
    /// Last build/reconfigure failure per vendor name, surfaced on
    /// `VendorView.error`. Cleared when that vendor next builds or
    /// reconfigures successfully.
    vendor_errors: RwLock<HashMap<String, String>>,
    /// Set once an *active* vendor's listener-affecting fields (`listen`/
    /// `advertise_host`/`server_url`) change — that one case still needs a
    /// process restart; never reset within a process's lifetime.
    restart_required: AtomicBool,
    info: ServerInfo,
    /// Held only to keep the shared local-runtime listener bound for the
    /// store's lifetime (unlike `velos_instances`, nothing reads this yet —
    /// no DB persistence, no live reconfigure, no listing endpoint).
    _local_daemon_registry: Option<LocalDaemonRegistry>,
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
        let (vendors, velos_instances) = build_vendors(&vendor_rows).await;

        let default_vendor = read_setting(&pool, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());
        let default_vendor = if vendors.contains_key(&default_vendor) {
            default_vendor
        } else {
            // A connected shared-local-vendor label isn't known at open()
            // time either (daemons dial in after startup), so fall back to
            // whatever vendor IS already loaded rather than hardcoding a
            // name that might not exist yet.
            let fallback = vendors
                .keys()
                .min()
                .cloned()
                .unwrap_or_else(|| "local".into());
            eprintln!(
                "warning: default vendor '{default_vendor}' is not loaded; using '{fallback}'"
            );
            fallback
        };

        let vendors: SharedVendors = Arc::new(RwLock::new(vendors));
        let local_daemon_registry = match deps.local_runtime_listen.as_deref() {
            Some(addr_str) => match addr_str.parse::<SocketAddr>() {
                Ok(addr) => match LocalDaemonRegistry::bind(addr, vendors.clone()).await {
                    Ok(registry) => Some(registry),
                    Err(e) => {
                        eprintln!("warning: shared local runtime vendor disabled: {e}");
                        None
                    }
                },
                Err(e) => {
                    eprintln!(
                        "warning: shared local runtime vendor disabled — invalid \
                         local_runtime_listen '{addr_str}': {e}"
                    );
                    None
                }
            },
            None => None,
        };
        let store = Arc::new(Self {
            pool: pool.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            velos_instances: RwLock::new(velos_instances),
            vendor_errors: RwLock::new(HashMap::new()),
            restart_required: AtomicBool::new(false),
            info: deps.info,
            _local_daemon_registry: local_daemon_registry,
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
            restart_required: self.restart_required.load(Ordering::Relaxed),
        })
    }

    fn vendors_view(&self, default_vendor: &str, rows: &[VendorRow]) -> Vec<VendorView> {
        let live = self.vendors.read().unwrap_or_else(|e| e.into_inner());
        let errors = self.vendor_errors.read().unwrap_or_else(|e| e.into_inner());
        let active = |name: &str| live.contains_key(name);
        let mut out = vec![VendorView {
            name: "local".into(),
            active: active("local"),
            is_default: default_vendor == "local",
            config: None,
            error: None,
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
                error: errors.get(&r.name).cloned(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// After vendor rows are persisted, bring the live vendor map in line with
    /// the new DB state: build newly-added or previously-inactive rows,
    /// live-reconfigure an active `velos` vendor whose listener-affecting
    /// fields are unchanged, leave an active vendor's old instance running
    /// (flagged) when those fields did change, and drop rows that were
    /// removed. Never fails the caller — outcomes land in `vendor_errors` /
    /// `restart_required` for the view to report.
    async fn reconcile_vendors(&self, before: &[VendorRow], after: &[VendorRow]) {
        let before_by_name: HashMap<&str, &VendorRow> =
            before.iter().map(|r| (r.name.as_str(), r)).collect();
        let after_names: HashSet<&str> = after.iter().map(|r| r.name.as_str()).collect();

        for name in before_by_name.keys().filter(|n| !after_names.contains(*n)) {
            self.vendors
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(*name);
            self.velos_instances
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(*name);
            self.vendor_errors
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(*name);
        }

        for row in after {
            let was_active = self
                .vendors
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&row.name);
            if was_active
                && let Some(prior) = before_by_name.get(row.name.as_str()).copied()
                && prior.kind == row.kind
            {
                self.apply_active_vendor_edit(row, prior).await;
            } else {
                self.activate_vendor(row).await;
            }
        }
    }

    /// A previously-active vendor of the same kind was edited: reconfigure it
    /// in place if the listener-affecting fields (`listen`/`advertise_host`/
    /// `server_url`) didn't change, else leave the running instance untouched
    /// and flag that vendor as needing a restart.
    async fn apply_active_vendor_edit(&self, row: &VendorRow, prior: &VendorRow) {
        if row.kind != "velos" {
            return;
        }
        let parsed = (
            serde_json::from_str::<VelosConfig>(&prior.config),
            serde_json::from_str::<VelosConfig>(&row.config),
        );
        let (Ok(old_vc), Ok(new_vc)) = parsed else {
            self.vendor_errors
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    row.name.clone(),
                    "stored config no longer parses".to_string(),
                );
            return;
        };
        let listener_unchanged = old_vc.listen == new_vc.listen
            && old_vc.advertise_host == new_vc.advertise_host
            && old_vc.server_url == new_vc.server_url;
        if listener_unchanged {
            match velos_mutable_settings(&new_vc) {
                Ok(settings) => {
                    let handle = self
                        .velos_instances
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&row.name)
                        .cloned();
                    if let Some(handle) = handle {
                        handle.reconfigure(settings);
                        self.vendor_errors
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&row.name);
                        return;
                    }
                }
                Err(e) => {
                    self.vendor_errors
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(row.name.clone(), e);
                    return;
                }
            }
        }
        self.restart_required.store(true, Ordering::Relaxed);
        self.vendor_errors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                row.name.clone(),
                "listen/advertise_host/server_url changed — restart the server to apply"
                    .to_string(),
            );
    }

    /// Bring a row online: a brand-new vendor, a previously-inactive one, or a
    /// kind change (which can't reuse an old listener, so it's rebuilt fresh).
    async fn activate_vendor(&self, row: &VendorRow) {
        match build_one_vendor(row).await {
            Ok(built) => {
                let BuiltVendor::Velos(v) = &built;
                self.velos_instances
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(row.name.clone(), v.clone());
                self.vendors
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(row.name.clone(), built.as_dyn());
                self.vendor_errors
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&row.name);
            }
            Err(e) => {
                self.vendor_errors
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(row.name.clone(), e);
            }
        }
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
                    "INSERT INTO providers (name, kind, base_url, api_key) VALUES (?, ?, ?, ?)",
                )
                .bind(name)
                .bind(&p.kind)
                .bind(trimmed(&p.base_url))
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

        let mut vendor_rows_before: Option<Vec<VendorRow>> = None;
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
            vendor_rows_before = Some(existing);
        }

        if let Some(dv) = &update.default_vendor {
            let (is_loaded, mut names) = {
                let loaded = self.vendors.read().unwrap_or_else(|e| e.into_inner());
                (
                    loaded.contains_key(dv),
                    loaded.keys().cloned().collect::<Vec<_>>(),
                )
            };
            if !is_loaded {
                names.sort();
                return Err(format!(
                    "vendor '{dv}' is not loaded (available: {})",
                    names.join(", ")
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
        if let Some(before) = vendor_rows_before {
            let after = read_vendors(&self.pool).await.map_err(|e| e.to_string())?;
            self.reconcile_vendors(&before, &after).await;
        }

        self.build_view().await
    }

    fn default_vendor(&self) -> String {
        self.default_vendor
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    async fn test_vendor(&self, name: &str) -> Result<VendorTestResult, String> {
        let rows = read_vendors(&self.pool).await.map_err(|e| e.to_string())?;
        let row = rows
            .into_iter()
            .find(|r| r.name == name)
            .ok_or_else(|| format!("unknown vendor '{name}'"))?;
        match row.kind.as_str() {
            "velos" => {
                let vc = serde_json::from_str::<VelosConfig>(&row.config)
                    .map_err(|e| format!("invalid config: {e}"))?;
                let token = resolve_velos_token(&vc)?;
                let client = VelosClient::new(&vc.server_url, token)
                    .map_err(|e| format!("velos client: {e}"))?;
                Ok(match client.whoami().await {
                    Ok(identity) => VendorTestResult {
                        ok: true,
                        identity: Some(identity),
                        error: None,
                    },
                    Err(VelosError::Status { status: 401, .. }) => VendorTestResult {
                        ok: false,
                        identity: None,
                        error: Some("token rejected (401 Unauthorized)".into()),
                    },
                    Err(e) => VendorTestResult {
                        ok: false,
                        identity: None,
                        error: Some(e.to_string()),
                    },
                })
            }
            other => Err(format!("vendor kind '{other}' does not support testing")),
        }
    }
}

// ── row types ────────────────────────────────────────────────────────────────

struct ProviderRow {
    name: String,
    kind: String,
    base_url: Option<String>,
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
    model_id: &str,
    max_tokens: Option<u32>,
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
    p = p.with_model(model_id).with_max_tokens(max_tokens);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}

/// A freshly built vendor, tagged so the caller can register it under both
/// the generic `vendors` map and (for kinds that support live reconfigure) a
/// concrete-typed side table — without ever downcasting a `dyn RuntimeVendor`.
enum BuiltVendor {
    Velos(Arc<VelosVendor>),
}

impl BuiltVendor {
    fn as_dyn(&self) -> Arc<dyn RuntimeVendor> {
        match self {
            BuiltVendor::Velos(v) => v.clone(),
        }
    }
}

/// Build one row's vendor instance, kind-dispatched. Used both at boot
/// (`build_vendors`'s loop) and per-row during a live config update.
async fn build_one_vendor(row: &VendorRow) -> Result<BuiltVendor, String> {
    match row.kind.as_str() {
        "velos" => {
            let vc = serde_json::from_str::<VelosConfig>(&row.config)
                .map_err(|e| format!("invalid config: {e}"))?;
            let vendor = build_velos_vendor(&vc).await?;
            Ok(BuiltVendor::Velos(Arc::new(vendor)))
        }
        other => Err(format!("unknown kind '{other}'")),
    }
}

/// Build the vendor set from configured rows. A vendor that fails to build
/// is logged and left out (reported inactive), never fatal — matches
/// `reconcile_vendors`'s per-update behavior. The shared local-runtime
/// vendor isn't built here: it's not a DB row, and its listener is bound
/// separately in `open()` (see [`LocalDaemonRegistry`]).
async fn build_vendors(
    rows: &[VendorRow],
) -> (
    HashMap<String, Arc<dyn RuntimeVendor>>,
    HashMap<String, Arc<VelosVendor>>,
) {
    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    let mut velos_instances: HashMap<String, Arc<VelosVendor>> = HashMap::new();
    for r in rows {
        match build_one_vendor(r).await {
            Ok(built) => {
                println!("vendor '{}' ({}) enabled", r.name, r.kind);
                let BuiltVendor::Velos(v) = &built;
                velos_instances.insert(r.name.clone(), v.clone());
                vendors.insert(r.name.clone(), built.as_dyn());
            }
            Err(e) => eprintln!("warning: vendor '{}' failed to start ({e})", r.name),
        }
    }
    (vendors, velos_instances)
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

/// Build a fresh `VelosMutableSettings` from a row's config — used by
/// `reconcile_vendors` to `reconfigure()` an already-bound vendor whose
/// listener-affecting fields didn't change.
fn velos_mutable_settings(vc: &VelosConfig) -> Result<VelosMutableSettings, String> {
    let token = resolve_velos_token(vc)?;
    let client =
        VelosClient::new(&vc.server_url, token).map_err(|e| format!("velos client: {e}"))?;
    Ok(VelosMutableSettings {
        api: Arc::new(client),
        image: vc.image.clone(),
        runtime_bin: vc.runtime_bin.clone(),
        workspace_root: vc.workspace_root.clone(),
        cpu: vc.cpu,
        memory_bytes: vc.memory_mib.saturating_mul(1024 * 1024),
        connect_timeout: Duration::from_secs(vc.connect_timeout_secs),
        public_http_base: vc
            .http_port
            .map(|p| format!("http://{}:{p}", vc.advertise_host)),
    })
}

fn resolve_velos_token(vc: &VelosConfig) -> Result<Option<Secret>, String> {
    match &vc.token {
        Some(t) if t.is_empty() => Err("velos inline token is empty".into()),
        Some(t) => Ok(Some(t.clone())),
        None => Ok(None),
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
    let rows = sqlx::query("SELECT name, kind, base_url, api_key FROM providers ORDER BY name")
        .fetch_all(ex)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ProviderRow {
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            base_url: r.try_get("base_url")?,
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
        let _ = dir; // kept for signature symmetry with other test helpers in this crate
        DbConfigStore::open(
            &format!("sqlite://{}/t.db", dir.display()),
            StoreDeps {
                info: info(),
                local_runtime_listen: None,
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

    fn velos_input(
        image: &str,
        listen: &str,
        http_port: Option<u32>,
        token: Option<&str>,
    ) -> VendorInput {
        VendorInput {
            name: "cluster-a".into(),
            config: VendorConfigInput::Velos(VelosInput {
                server_url: "http://velos:8080".into(),
                image: image.into(),
                advertise_host: "10.0.0.5".into(),
                token: token.map(str::to_string),
                runtime_bin: None,
                workspace_root: None,
                listen: Some(listen.into()),
                cpu: None,
                memory_mib: None,
                connect_timeout_secs: None,
                http_port,
            }),
        }
    }

    #[tokio::test]
    async fn new_vendor_activates_live_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input(
                    "img",
                    "127.0.0.1:0",
                    None,
                    Some("secret"),
                )]),
                default_vendor: None,
            })
            .await
            .expect("velos update ok");
        assert!(!view.restart_required);
        let v = view
            .vendors
            .iter()
            .find(|v| v.name == "cluster-a")
            .expect("present");
        assert!(v.active, "a valid new vendor activates immediately");
        assert!(v.error.is_none());
        match &v.config {
            Some(VendorConfigView::Velos(velos)) => {
                assert!(velos.has_inline_token);
                assert_eq!(velos.runtime_bin, "horsie-runtime"); // default applied
            }
            None => panic!("expected velos config"),
        }
        assert!(o.vendors.read().unwrap().contains_key("cluster-a"));
    }

    #[tokio::test]
    async fn not_yet_active_vendor_build_failure_reports_error_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;

        // Occupy a port so the vendor's listener bind fails deterministically
        // (a real "bad token" only surfaces on the vendor's first actual velos
        // API call, not at build time — an unbindable listen is the reliable,
        // portable way to force a build-time failure here).
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = blocker.local_addr().unwrap().to_string();

        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input("img", &busy, None, Some("secret"))]),
                default_vendor: None,
            })
            .await
            .expect("persists even though the vendor fails to build");
        let v = view
            .vendors
            .iter()
            .find(|v| v.name == "cluster-a")
            .expect("present");
        assert!(!v.active);
        assert!(v.error.is_some());
        assert!(!view.restart_required);

        drop(blocker);
        let view2 = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input("img", "127.0.0.1:0", None, None)]),
                default_vendor: None,
            })
            .await
            .expect("second update ok");
        let v2 = view2
            .vendors
            .iter()
            .find(|v| v.name == "cluster-a")
            .expect("present");
        assert!(v2.active);
        assert!(v2.error.is_none());
    }

    #[tokio::test]
    async fn active_vendor_non_listener_edit_applies_live() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input(
                    "img-v1",
                    "127.0.0.1:0",
                    None,
                    Some("secret"),
                )]),
                default_vendor: None,
            })
            .await
            .unwrap();
        assert!(o.vendors.read().unwrap().contains_key("cluster-a"));

        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input("img-v2", "127.0.0.1:0", Some(9000), None)]),
                default_vendor: None,
            })
            .await
            .unwrap();

        assert!(!view.restart_required);
        let handle = o
            .store
            .velos_instances
            .read()
            .unwrap()
            .get("cluster-a")
            .cloned()
            .expect("still the live instance");
        let settings = handle.settings();
        assert_eq!(settings.image, "img-v2");
        assert_eq!(
            settings.public_http_base.as_deref(),
            Some("http://10.0.0.5:9000")
        );
    }

    #[tokio::test]
    async fn active_vendor_listener_edit_requires_restart_and_keeps_old_instance() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input(
                    "img",
                    "127.0.0.1:0",
                    None,
                    Some("secret"),
                )]),
                default_vendor: None,
            })
            .await
            .unwrap();
        let before = o
            .store
            .velos_instances
            .read()
            .unwrap()
            .get("cluster-a")
            .cloned()
            .unwrap();

        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input("img", "127.0.0.1:4551", None, None)]),
                default_vendor: None,
            })
            .await
            .unwrap();

        assert!(view.restart_required);
        let v = view.vendors.iter().find(|v| v.name == "cluster-a").unwrap();
        assert!(v.active, "the old instance keeps serving");
        assert!(v.error.as_deref().unwrap_or_default().contains("restart"));
        let after = o
            .store
            .velos_instances
            .read()
            .unwrap()
            .get("cluster-a")
            .cloned()
            .unwrap();
        assert!(Arc::ptr_eq(&before, &after), "no rebuild happened");
    }

    #[tokio::test]
    async fn removed_vendor_row_drops_from_live_map() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![velos_input(
                    "img",
                    "127.0.0.1:0",
                    None,
                    Some("secret"),
                )]),
                default_vendor: None,
            })
            .await
            .unwrap();
        assert!(o.vendors.read().unwrap().contains_key("cluster-a"));

        let view = o
            .store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![]),
                default_vendor: None,
            })
            .await
            .unwrap();

        assert!(!o.vendors.read().unwrap().contains_key("cluster-a"));
        assert!(view.vendors.iter().all(|v| v.name != "cluster-a"));
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

    #[tokio::test]
    async fn build_one_vendor_reports_unknown_kind() {
        let row = VendorRow {
            name: "x".into(),
            kind: "bogus".into(),
            config: "{}".into(),
        };
        let err = match build_one_vendor(&row).await {
            Err(e) => e,
            Ok(_) => panic!("unknown kind must be rejected"),
        };
        assert!(err.contains("bogus"), "{err}");
    }

    #[tokio::test]
    async fn build_one_vendor_velos_returns_arc_dyn_runtime_vendor() {
        let row = VendorRow {
            name: "cluster-a".into(),
            kind: "velos".into(),
            config: serde_json::json!({
                "server_url": "http://velos:8080",
                "image": "img",
                "advertise_host": "10.0.0.5",
                "listen": "127.0.0.1:0",
            })
            .to_string(),
        };
        let built = build_one_vendor(&row).await.expect("velos row builds");
        assert_eq!(built.as_dyn().name(), "velos");
    }

    // A tiny mock velos server exposing just `/auth/v1/me`, for `test_vendor`.
    async fn spawn_mock_velos(accept_token: &str) -> String {
        use axum::extract::State as AxumState;
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::get;

        #[derive(Clone)]
        struct S {
            accept: std::sync::Arc<String>,
        }
        async fn whoami(AxumState(s): AxumState<S>, headers: HeaderMap) -> impl IntoResponse {
            let ok = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("Bearer {}", s.accept))
                .unwrap_or(false);
            if ok {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({ "identity": "admin" })),
                )
            } else {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({ "error": "unauthorized" })),
                )
            }
        }
        let state = S {
            accept: std::sync::Arc::new(accept_token.to_string()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/auth/v1/me", get(whoami))
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_vendor_reports_ok_for_a_good_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let base = spawn_mock_velos("good-token").await;
        let mut input = velos_input("img", "127.0.0.1:0", None, Some("good-token"));
        let VendorConfigInput::Velos(v) = &mut input.config;
        v.server_url = base;
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![input]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        let result = o.store.test_vendor("cluster-a").await.expect("test ran");
        assert!(result.ok);
        assert_eq!(result.identity.as_deref(), Some("admin"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_vendor_reports_error_for_a_bad_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let base = spawn_mock_velos("good-token").await;
        let mut input = velos_input("img", "127.0.0.1:0", None, Some("wrong-token"));
        let VendorConfigInput::Velos(v) = &mut input.config;
        v.server_url = base;
        o.store
            .update(SettingsUpdate {
                providers: None,
                models: None,
                vendors: Some(vec![input]),
                default_vendor: None,
            })
            .await
            .expect("update ok");

        let result = o.store.test_vendor("cluster-a").await.expect("test ran");
        assert!(!result.ok);
        assert!(result.identity.is_none());
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_vendor_errors_for_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let err = o.store.test_vendor("ghost").await.unwrap_err();
        assert!(err.contains("ghost"), "error names the vendor: {err}");
    }
}
