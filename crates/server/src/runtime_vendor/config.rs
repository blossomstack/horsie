//! Configured runtime vendors: the `runtime_vendors` table, and the service
//! that turns its rows into live [`RuntimeVendor`]s.
//!
//! A `horsie connect` vendor announces itself over a websocket, so the server
//! never has to remember it. A cloud vendor has nowhere to dial in *from* — its
//! configuration is the only evidence it exists — so it is stored, and rebuilt
//! into the vendor map at boot and on every edit.
//!
//! **Both kinds share one map.** Sessions select a vendor by name and must not
//! care which sort it is, which is the point of the trait. Names are therefore
//! exclusive across both: see [`RuntimeVendorRegistry::register`], which refuses
//! a dialled-in agent the name of a configured vendor.
//!
//! [`RuntimeVendorRegistry::register`]: crate::runtime_vendor::RuntimeVendorRegistry::register

use crate::auth::UserId;
use crate::db::Db;
use crate::runtime_vendor::fly::{FlyRuntimeVendor, FlySettings};
use crate::runtime_vendor::fly_api::{FlyHttpApi, FlyMachineSize};
use crate::runtime_vendor::velos::{VelosRuntimeVendor, VelosSettings};
use crate::runtime_vendor::{RuntimeVendor, WebsocketVendorTable};
use crate::sessions::spec::RuntimeVendorMap;
use horsie_runtime_vendor::ConnectedRuntimeRegistry;
use sqlx::Row;
use sqlx::any::AnyRow;
use std::sync::{Arc, PoisonError};

const COLS: &str = "name, kind, settings, credential, created_at, updated_at";

/// The path on this server that runtimes dial. Appended when an operator gives
/// a bare origin, which is the shape they will reach for.
const CONNECT_PATH: &str = "/api/runtime/connect";

/// How a Fly vendor builds machines: the *storage* shape.
///
/// A twin of the wire `runtime_vendor::FlyVendorSettings` rather than the type
/// itself, following the same rule the environments store follows. It is what
/// makes a wire rename a compile error in [`from_wire`](StoredVendorSettings::from_wire)
/// instead of a silent parse failure that makes every configured vendor
/// disappear at the next boot.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredFlySettings {
    /// The Fly app machines are created in. Must already exist — this server
    /// creates machines, not apps.
    pub app: String,
    /// OCI image with `horsie-runtime` baked in.
    pub image: String,
    pub region: String,
    /// Where in the machine workspaces are allocated.
    pub workspace_root: String,
    /// The `ws://`/`wss://` URL a machine reaches this server on.
    pub callback_url: String,
    /// Give each runtime a volume, so a stopped one keeps its workspace.
    pub volumes: bool,
    pub cpu_kind: String,
    pub cpus: u32,
    pub memory_mb: u32,
    pub volume_size_gb: u32,
}

impl Default for StoredFlySettings {
    fn default() -> Self {
        Self {
            app: String::new(),
            image: String::new(),
            region: "iad".to_string(),
            workspace_root: "/workspaces".to_string(),
            callback_url: String::new(),
            volumes: true,
            cpu_kind: "shared".to_string(),
            cpus: 1,
            memory_mb: 1024,
            volume_size_gb: 10,
        }
    }
}

/// How a velos vendor schedules containers: the *storage* shape. A twin of the
/// wire `runtime_vendor::VelosVendorSettings`, for the reason on
/// [`StoredFlySettings`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredVelosSettings {
    pub server_url: String,
    pub image: String,
    pub runtime_bin: String,
    pub workspace_root: String,
    pub callback_url: String,
    pub cpu: u32,
    pub memory_mb: u32,
}

impl Default for StoredVelosSettings {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            image: String::new(),
            runtime_bin: "horsie-runtime".to_string(),
            workspace_root: "/workspaces".to_string(),
            callback_url: String::new(),
            cpu: 1,
            memory_mb: 1024,
        }
    }
}

/// One row of `runtime_vendors`, with `settings` already parsed by `kind`.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeVendorRow {
    pub name: String,
    pub settings: StoredVendorSettings,
    /// The vendor API token.
    pub credential: String,
    pub created_at: String,
    pub updated_at: String,
}

impl RuntimeVendorRow {
    /// The client's view. The credential is reported as present or absent and
    /// never returned: a settings page that could read a token back turns every
    /// session cookie into a way to steal one.
    #[must_use]
    pub fn to_view(&self) -> horsie_models::runtime_vendor::RuntimeVendorConfigView {
        horsie_models::runtime_vendor::RuntimeVendorConfigView {
            name: self.name.clone(),
            settings: self.settings.to_wire(),
            has_credential: !self.credential.is_empty(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

/// Why a configured-vendor edit was refused.
#[derive(Debug)]
pub enum VendorConfigError {
    NotFound(String),
    /// The name belongs to a dialled-in agent.
    Conflict(String),
    Invalid(String),
    Internal(String),
}

/// Every vendor kind the server can build. One variant per `kind` value.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredVendorSettings {
    Fly(StoredFlySettings),
    Velos(StoredVelosSettings),
}

impl StoredVendorSettings {
    /// The `kind` column value. The enum is the source of truth for the string,
    /// so a new variant cannot forget to name itself.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fly(_) => "fly",
            Self::Velos(_) => "velos",
        }
    }

    fn to_json(&self) -> Result<String, String> {
        match self {
            Self::Fly(s) => serde_json::to_string(s).map_err(|e| e.to_string()),
            Self::Velos(s) => serde_json::to_string(s).map_err(|e| e.to_string()),
        }
    }

    fn parse(kind: &str, json: &str) -> Result<Self, String> {
        match kind {
            "fly" => serde_json::from_str(json)
                .map(Self::Fly)
                .map_err(|e| format!("runtime_vendors.settings: {e}")),
            "velos" => serde_json::from_str(json)
                .map(Self::Velos)
                .map_err(|e| format!("runtime_vendors.settings: {e}")),
            other => Err(format!("unknown runtime vendor kind '{other}'")),
        }
    }

    #[must_use]
    pub fn from_wire(wire: horsie_models::runtime_vendor::RuntimeVendorSettings) -> Self {
        use horsie_models::runtime_vendor::RuntimeVendorSettings as Wire;
        match wire {
            Wire::Fly(f) => Self::Fly(StoredFlySettings {
                app: f.app,
                image: f.image,
                region: f.region,
                workspace_root: f.workspace_root,
                callback_url: f.callback_url,
                volumes: f.volumes,
                cpu_kind: f.cpu_kind,
                cpus: f.cpus,
                memory_mb: f.memory_mb,
                volume_size_gb: f.volume_size_gb,
            }),
            Wire::Velos(v) => Self::Velos(StoredVelosSettings {
                server_url: v.server_url,
                image: v.image,
                runtime_bin: v.runtime_bin,
                workspace_root: v.workspace_root,
                callback_url: v.callback_url,
                cpu: v.cpu,
                memory_mb: v.memory_mb,
            }),
        }
    }

    #[must_use]
    pub fn to_wire(&self) -> horsie_models::runtime_vendor::RuntimeVendorSettings {
        use horsie_models::runtime_vendor as wire;
        match self {
            Self::Fly(f) => wire::RuntimeVendorSettings::Fly(wire::FlyVendorSettings {
                app: f.app.clone(),
                image: f.image.clone(),
                region: f.region.clone(),
                workspace_root: f.workspace_root.clone(),
                callback_url: f.callback_url.clone(),
                volumes: f.volumes,
                cpu_kind: f.cpu_kind.clone(),
                cpus: f.cpus,
                memory_mb: f.memory_mb,
                volume_size_gb: f.volume_size_gb,
            }),
            Self::Velos(v) => wire::RuntimeVendorSettings::Velos(wire::VelosVendorSettings {
                server_url: v.server_url.clone(),
                image: v.image.clone(),
                runtime_bin: v.runtime_bin.clone(),
                workspace_root: v.workspace_root.clone(),
                callback_url: v.callback_url.clone(),
                cpu: v.cpu,
                memory_mb: v.memory_mb,
            }),
        }
    }
}

/// Reject a configuration that cannot possibly work, at save time.
///
/// The alternative is a vendor that saves cleanly and then fails every session
/// with a timeout, because a machine on Fly dialled a hostname that only
/// resolves on the server's own loopback. That failure surfaces minutes later,
/// in a session, attributed to nothing.
///
/// Returns the normalised callback URL.
pub fn validate(settings: &StoredVendorSettings, credential: &str) -> Result<String, String> {
    match settings {
        StoredVendorSettings::Fly(fly) => {
            if credential.trim().is_empty() {
                return Err("a fly vendor needs an API token".to_string());
            }
            for (field, value) in [("app", &fly.app), ("image", &fly.image)] {
                if value.trim().is_empty() {
                    return Err(format!("a fly vendor needs {field}"));
                }
            }
            if fly.cpus == 0 || fly.memory_mb == 0 {
                return Err("a machine needs at least one cpu and some memory".to_string());
            }
            if fly.volumes && fly.volume_size_gb == 0 {
                return Err("a volume needs a size".to_string());
            }
            normalise_callback(&fly.callback_url)
        }
        StoredVendorSettings::Velos(velos) => {
            // No credential check: a velos deployment may run without auth, and
            // demanding a token for one would make it unconfigurable.
            for (field, value) in [
                ("a server url", &velos.server_url),
                ("an image", &velos.image),
                ("a runtime binary path", &velos.runtime_bin),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("a velos vendor needs {field}"));
                }
            }
            if !velos.server_url.starts_with("http://") && !velos.server_url.starts_with("https://")
            {
                return Err("the velos server url must start with http:// or https://".to_string());
            }
            if velos.cpu == 0 || velos.memory_mb == 0 {
                return Err("a container needs at least one cpu and some memory".to_string());
            }
            normalise_callback(&velos.callback_url)
        }
    }
}

/// Check a callback URL a machine has to reach from outside, and fill in the
/// connect path when an operator gives a bare origin.
pub fn normalise_callback(url: &str) -> Result<String, String> {
    let url = url.trim();
    let Some(rest) = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
    else {
        return Err("the callback url must start with ws:// or wss://".to_string());
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .trim_matches(['[', ']']);
    if host.is_empty() {
        return Err("the callback url has no host".to_string());
    }
    // A sandbox on someone else's infrastructure resolves these to itself, so a
    // vendor configured this way can never work — and would fail as a silent
    // timeout rather than as an error anyone can act on.
    if matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1") || host.ends_with(".localhost")
    {
        return Err(format!(
            "a machine cannot reach '{host}' — the callback url must be an address reachable from outside this server"
        ));
    }
    // `trim_end_matches`, not a bare concatenation: a url written with a
    // trailing slash has an empty path, and appending to it produced
    // `wss://host//api/runtime/connect`, which axum will not route — so every
    // runtime dialled a 404 and the vendor looked broken for a stray keystroke.
    Ok(if path.is_empty() {
        format!("{}{CONNECT_PATH}", url.trim_end_matches('/'))
    } else {
        url.to_string()
    })
}

pub struct RuntimeVendorStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: UserId,
}

impl RuntimeVendorStore {
    #[must_use]
    pub fn new(db: Db, user: UserId) -> Self {
        Self { db, user }
    }

    pub async fn list(&self) -> Result<Vec<RuntimeVendorRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM runtime_vendors WHERE user_id = ? ORDER BY name"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_vendor).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<RuntimeVendorRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM runtime_vendors WHERE user_id = ? AND name = ?"
        )))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_vendor).transpose()
    }

    /// Insert or replace. Unlike environments this *is* an upsert: a vendor row
    /// is a connection setting rather than a document, and re-saving one is how
    /// a rotated token is applied.
    pub async fn upsert(&self, row: &RuntimeVendorRow) -> Result<(), String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(
            &self
                .db
                .q("DELETE FROM runtime_vendors WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(&row.name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO runtime_vendors (user_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.user.as_str())
        .bind(&row.name)
        .bind(row.settings.kind())
        .bind(row.settings.to_json()?)
        .bind(&row.credential)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("save runtime vendor '{}': {e}", row.name))?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM runtime_vendors WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

/// Unix epoch seconds as text, matching every other timestamp column.
fn unix_seconds() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn row_to_vendor(row: &AnyRow) -> Result<RuntimeVendorRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(RuntimeVendorRow {
        name: get("name")?,
        settings: StoredVendorSettings::parse(&get("kind")?, &get("settings")?)?,
        credential: get("credential")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

/// Owns the configured vendors: the rows, and their published counterparts.
///
/// Publication is unconditional and immediate — a saved vendor is selectable on
/// the next session, with no restart. That is only possible because a
/// configured vendor needs no listener of its own: its runtimes dial the
/// server's own connect route.
pub struct RuntimeVendorConfigService {
    store: RuntimeVendorStore,
    account: String,
    /// The map sessions select from, shared with dialled-in vendors.
    vendors: RuntimeVendorMap,
    /// Which names belong to a dialled-in agent. Consulted so a save cannot
    /// take a live agent's name out from under it.
    websockets: WebsocketVendorTable,
    connected: Arc<ConnectedRuntimeRegistry>,
    dial_secret: Arc<Vec<u8>>,
}

impl RuntimeVendorConfigService {
    #[must_use]
    pub fn new(
        store: RuntimeVendorStore,
        account: String,
        vendors: RuntimeVendorMap,
        websockets: WebsocketVendorTable,
        connected: Arc<ConnectedRuntimeRegistry>,
        dial_secret: Arc<Vec<u8>>,
    ) -> Self {
        Self {
            store,
            account,
            vendors,
            websockets,
            connected,
            dial_secret,
        }
    }

    pub async fn list(&self) -> Result<Vec<RuntimeVendorRow>, String> {
        self.store.list().await
    }

    /// The HTTP surface: list, save, delete in wire types.
    pub async fn list_views(
        &self,
    ) -> Result<Vec<horsie_models::runtime_vendor::RuntimeVendorConfigView>, VendorConfigError>
    {
        self.store
            .list()
            .await
            .map(|rows| rows.iter().map(RuntimeVendorRow::to_view).collect())
            .map_err(VendorConfigError::Internal)
    }

    /// Save what a client sent.
    ///
    /// An omitted credential keeps the stored one, so editing a region does not
    /// require re-typing a token the client was never allowed to read back.
    pub async fn save_input(
        &self,
        name: &str,
        input: horsie_models::runtime_vendor::RuntimeVendorConfigInput,
    ) -> Result<horsie_models::runtime_vendor::RuntimeVendorConfigView, VendorConfigError> {
        if input.name != name {
            return Err(VendorConfigError::Invalid(
                "the name in the body must match the one in the path".to_string(),
            ));
        }
        let existing = self
            .store
            .get(name)
            .await
            .map_err(VendorConfigError::Internal)?;
        let credential = match (input.credential, existing.as_ref()) {
            (Some(c), _) => c,
            (None, Some(row)) => row.credential.clone(),
            (None, None) => {
                return Err(VendorConfigError::Invalid(
                    "a new runtime vendor needs a credential".to_string(),
                ));
            }
        };
        let now = unix_seconds();
        let row = RuntimeVendorRow {
            name: input.name,
            settings: StoredVendorSettings::from_wire(input.settings),
            credential,
            created_at: existing.map_or_else(|| now.clone(), |r| r.created_at),
            updated_at: now,
        };
        self.save(row).await.map(|r| r.to_view()).map_err(|e| {
            // A name held by a live agent is the one refusal a client can act
            // on by choosing another name, so it is not lumped in with the rest.
            if e.contains("connected vendor process") {
                VendorConfigError::Conflict(e)
            } else {
                VendorConfigError::Invalid(e)
            }
        })
    }

    /// Ask every vendor to destroy what it still holds for sessions that are
    /// gone, and log what went.
    ///
    /// Sweeps the whole vendor map, not just the configured rows: the trait
    /// method defaults to doing nothing, so a vendor that cannot inventory
    /// itself opts out by saying nothing rather than by being excluded here.
    ///
    /// `live` must be *every* session the server knows, loaded or not. A short
    /// list here is a destroyed workspace, so a caller that cannot produce the
    /// full set must not call this at all.
    pub async fn sweep_orphans(&self, live: &std::collections::HashSet<String>) {
        // Cloned out of the lock: a sweep makes network calls, and holding a
        // std::sync lock across an await is not an option.
        let vendors: Vec<(String, Arc<dyn RuntimeVendor>)> = self
            .vendors
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, v)| (name.clone(), v.clone()))
            .collect();
        for (name, vendor) in vendors {
            match vendor.sweep_orphans(live).await {
                Ok(swept) if swept.is_empty() => {}
                Ok(swept) => tracing::info!(
                    vendor = %name,
                    count = swept.len(),
                    runtimes = ?swept,
                    "swept runtimes whose sessions are gone"
                ),
                // Never fatal: an unreachable vendor is swept on the next boot,
                // and the cost of skipping is a machine that bills for longer.
                Err(e) => tracing::warn!(vendor = %name, error = %e, "orphan sweep failed"),
            }
        }
    }

    pub async fn delete_named(&self, name: &str) -> Result<(), VendorConfigError> {
        match self.delete(name).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(VendorConfigError::NotFound(format!(
                "no runtime vendor named '{name}'"
            ))),
            Err(e) => Err(VendorConfigError::Internal(e)),
        }
    }

    /// Build and publish every stored vendor. Called once at boot.
    ///
    /// A row that cannot be built is logged and skipped rather than failing the
    /// account: one malformed vendor must not stop a user reaching the settings
    /// page that would let them fix it.
    pub async fn publish_all(&self) {
        let rows = match self.store.list().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "reading configured runtime vendors failed");
                return;
            }
        };
        for row in rows {
            if let Err(e) = self.publish(&row) {
                tracing::warn!(vendor = %row.name, error = %e, "a configured runtime vendor could not be built");
            }
        }
    }

    /// Validate, store, and publish. The published vendor replaces any previous
    /// one under the same name, which is how an edited token takes effect.
    pub async fn save(&self, row: RuntimeVendorRow) -> Result<RuntimeVendorRow, String> {
        if row.name.trim().is_empty() {
            return Err("a runtime vendor needs a name".to_string());
        }
        if self.is_websocket_name(&row.name) {
            return Err(format!(
                "the name '{}' is in use by a connected vendor process",
                row.name
            ));
        }
        let callback = validate(&row.settings, &row.credential)?;
        let settings = match row.settings {
            StoredVendorSettings::Fly(fly) => StoredVendorSettings::Fly(StoredFlySettings {
                callback_url: callback,
                ..fly
            }),
            StoredVendorSettings::Velos(velos) => {
                StoredVendorSettings::Velos(StoredVelosSettings {
                    callback_url: callback,
                    ..velos
                })
            }
        };
        let row = RuntimeVendorRow { settings, ..row };
        self.store.upsert(&row).await?;
        self.publish(&row)?;
        Ok(row)
    }

    /// Forget a vendor and unpublish it.
    ///
    /// Machines it created are left running: this server is no longer able to
    /// reach them, so destroying them would need a credential it is being told
    /// to forget. Deleting sessions is what reclaims machines.
    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let removed = self.store.delete(name).await?;
        if removed {
            self.vendors
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(name);
        }
        Ok(removed)
    }

    fn is_websocket_name(&self, name: &str) -> bool {
        self.websockets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(name)
    }

    fn publish(&self, row: &RuntimeVendorRow) -> Result<(), String> {
        let vendor = self.build(row)?;
        self.vendors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(row.name.clone(), vendor);
        Ok(())
    }

    /// Turn one stored row into a live vendor.
    ///
    /// Fallible because a velos client validates its URL up front. A Fly vendor
    /// cannot fail to build at all — a bad token surfaces on the first API call
    /// rather than here — which is the difference between a client that dials
    /// and one that is only a `reqwest::Client` and a string.
    fn build(&self, row: &RuntimeVendorRow) -> Result<Arc<dyn RuntimeVendor>, String> {
        match &row.settings {
            StoredVendorSettings::Fly(fly) => {
                let api = FlyHttpApi::new(
                    fly.app.clone(),
                    row.credential.clone(),
                    FlyMachineSize {
                        cpu_kind: fly.cpu_kind.clone(),
                        cpus: fly.cpus,
                        memory_mb: fly.memory_mb,
                        volume_size_gb: fly.volume_size_gb,
                    },
                );
                Ok(Arc::new(FlyRuntimeVendor::new(
                    row.name.clone(),
                    api,
                    FlySettings {
                        image: fly.image.clone(),
                        region: fly.region.clone(),
                        workspace_root: fly.workspace_root.clone(),
                        callback_url: fly.callback_url.clone(),
                        volumes: fly.volumes,
                    },
                    self.connected.clone(),
                    self.dial_secret.clone(),
                    self.account.clone(),
                )))
            }
            StoredVendorSettings::Velos(velos) => {
                // An empty credential means a velos deployment without auth,
                // which is a supported configuration — so it becomes no bearer
                // rather than an empty one.
                let token = (!row.credential.trim().is_empty())
                    .then(|| horsie_agentcore::Secret::from(row.credential.clone()));
                let api = crate::runtime_vendor::velos_api::VelosClient::new(
                    velos.server_url.clone(),
                    token,
                )
                .map_err(|e| e.to_string())?;
                Ok(Arc::new(VelosRuntimeVendor::new(
                    row.name.clone(),
                    Arc::new(api),
                    VelosSettings {
                        image: velos.image.clone(),
                        runtime_bin: velos.runtime_bin.clone(),
                        workspace_root: velos.workspace_root.clone(),
                        callback_url: velos.callback_url.clone(),
                        cpu: velos.cpu,
                        memory_bytes: u64::from(velos.memory_mb) * 1024 * 1024,
                    },
                    self.connected.clone(),
                    self.dial_secret.clone(),
                    self.account.clone(),
                )))
            }
        }
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

    fn settings() -> StoredVendorSettings {
        StoredVendorSettings::Fly(StoredFlySettings {
            app: "horsie-runtimes".to_string(),
            image: "ghcr.io/x/runtime:1".to_string(),
            callback_url: "wss://horsie.example.com".to_string(),
            ..StoredFlySettings::default()
        })
    }

    fn row() -> RuntimeVendorRow {
        RuntimeVendorRow {
            name: "fly".to_string(),
            settings: settings(),
            credential: "fly-token".to_string(),
            created_at: "1".to_string(),
            updated_at: "1".to_string(),
        }
    }

    fn service(vendors: RuntimeVendorMap, db: Db) -> RuntimeVendorConfigService {
        RuntimeVendorConfigService::new(
            RuntimeVendorStore::new(db, UserId::new("u1")),
            "u1".to_string(),
            vendors,
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(vec![0_u8; 32]),
        )
    }

    fn empty_map() -> RuntimeVendorMap {
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
    }

    #[test]
    fn a_bare_origin_gains_the_connect_path() {
        // The shape an operator reaches for; without this the machine dials the
        // server root and gets a 404 it cannot explain.
        assert_eq!(
            normalise_callback("wss://horsie.example.com").unwrap(),
            "wss://horsie.example.com/api/runtime/connect"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        // A stray keystroke used to produce `//api/runtime/connect`, which axum
        // will not route — so every runtime dialled a 404 and the vendor looked
        // broken rather than mistyped.
        assert_eq!(
            normalise_callback("wss://horsie.example.com/").unwrap(),
            "wss://horsie.example.com/api/runtime/connect"
        );
    }

    #[test]
    fn an_explicit_path_is_left_alone() {
        assert_eq!(
            normalise_callback("wss://horsie.example.com/relay/rt").unwrap(),
            "wss://horsie.example.com/relay/rt"
        );
    }

    #[test]
    fn a_loopback_callback_is_refused() {
        // The whole point of validating at save time: this configuration saves
        // cleanly and then fails every session as an unexplained timeout.
        for url in [
            "ws://localhost:8080",
            "ws://127.0.0.1:8080/api/runtime/connect",
            "wss://[::1]:8080",
            "ws://0.0.0.0:8080",
            "ws://app.localhost",
        ] {
            assert!(normalise_callback(url).is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn a_non_websocket_scheme_is_refused() {
        // The runtime's own endpoint parser accepts only ws/wss, so an https
        // URL would fail inside the machine where nobody can see it.
        assert!(normalise_callback("https://horsie.example.com").is_err());
        assert!(normalise_callback("horsie.example.com").is_err());
    }

    #[test]
    fn an_incomplete_vendor_is_refused() {
        let StoredVendorSettings::Fly(fly) = settings() else {
            panic!("the fixture is a fly vendor")
        };
        assert!(validate(&settings(), "").is_err(), "no token");
        assert!(
            validate(
                &StoredVendorSettings::Fly(StoredFlySettings {
                    app: String::new(),
                    ..fly.clone()
                }),
                "t"
            )
            .is_err(),
            "no app"
        );
        assert!(
            validate(
                &StoredVendorSettings::Fly(StoredFlySettings { cpus: 0, ..fly }),
                "t"
            )
            .is_err(),
            "no cpus"
        );
    }

    fn velos_settings() -> StoredVendorSettings {
        StoredVendorSettings::Velos(StoredVelosSettings {
            server_url: "http://velos:8080".to_string(),
            image: "ghcr.io/x/runtime:1".to_string(),
            callback_url: "ws://horsie.internal:8080".to_string(),
            ..StoredVelosSettings::default()
        })
    }

    #[tokio::test]
    async fn a_velos_vendor_is_saved_and_published_like_any_other() {
        // The point of the union: a second kind is a variant and a match arm,
        // and every path above it — validate, store, publish — is unchanged.
        let db = crate::db::testing::db().await;
        let vendors = empty_map();
        let service = service(vendors.clone(), db);
        service
            .save(RuntimeVendorRow {
                name: "velos".to_string(),
                settings: velos_settings(),
                credential: String::new(),
                ..row()
            })
            .await
            .unwrap();
        assert!(vendors.read().unwrap().contains_key("velos"));
    }

    #[tokio::test]
    async fn a_velos_vendor_needs_no_credential() {
        // A velos deployment may run without auth. Demanding a token would make
        // one unconfigurable, so an empty credential becomes no bearer.
        assert!(validate(&velos_settings(), "").is_ok());
        // A fly vendor is the opposite: its API is never anonymous.
        assert!(validate(&settings(), "").is_err());
    }

    #[test]
    fn a_velos_server_url_must_be_http() {
        let StoredVendorSettings::Velos(v) = velos_settings() else {
            panic!("the fixture is a velos vendor")
        };
        assert!(
            validate(
                &StoredVendorSettings::Velos(StoredVelosSettings {
                    server_url: "velos:8080".to_string(),
                    ..v
                }),
                ""
            )
            .is_err()
        );
    }

    #[test]
    fn settings_round_trip_through_their_kind() {
        let json = settings().to_json().unwrap();
        assert_eq!(
            StoredVendorSettings::parse("fly", &json).unwrap(),
            settings()
        );
        assert!(StoredVendorSettings::parse("e2b", &json).is_err());
    }

    #[tokio::test]
    async fn a_saved_vendor_is_selectable_without_a_restart() {
        let db = crate::db::testing::db().await;
        let vendors = empty_map();
        let service = service(vendors.clone(), db);
        service.save(row()).await.unwrap();
        assert!(
            vendors.read().unwrap().contains_key("fly"),
            "a saved vendor must be published immediately"
        );
    }

    #[tokio::test]
    async fn a_stored_vendor_is_rebuilt_at_boot() {
        // The reason the table exists: a cloud vendor has nothing to dial in
        // from, so nothing would re-announce it.
        let db = crate::db::testing::db().await;
        service(empty_map(), db.clone()).save(row()).await.unwrap();

        let vendors = empty_map();
        service(vendors.clone(), db).publish_all().await;
        assert_eq!(
            vendors
                .read()
                .unwrap()
                .get("fly")
                .map(|v| v.name().to_string()),
            Some("fly".to_string())
        );
    }

    #[tokio::test]
    async fn saving_normalises_the_callback_before_storing_it() {
        let db = crate::db::testing::db().await;
        let service = service(empty_map(), db);
        let saved = service.save(row()).await.unwrap();
        let StoredVendorSettings::Fly(fly) = saved.settings else {
            panic!("a fly vendor must round-trip as one")
        };
        assert_eq!(
            fly.callback_url,
            "wss://horsie.example.com/api/runtime/connect"
        );
    }

    #[tokio::test]
    async fn a_delete_unpublishes_the_vendor() {
        let db = crate::db::testing::db().await;
        let vendors = empty_map();
        let service = service(vendors.clone(), db);
        service.save(row()).await.unwrap();
        assert!(service.delete("fly").await.unwrap());
        assert!(vendors.read().unwrap().is_empty());
        assert!(
            !service.delete("fly").await.unwrap(),
            "delete is idempotent"
        );
    }

    #[tokio::test]
    async fn a_save_cannot_steal_a_connected_agents_name() {
        let db = crate::db::testing::db().await;
        let websockets: WebsocketVendorTable =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let service = RuntimeVendorConfigService::new(
            RuntimeVendorStore::new(db, UserId::new("u1")),
            "u1".to_string(),
            empty_map(),
            websockets.clone(),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(vec![0_u8; 32]),
        );
        // A real dialled-in agent holding the name.
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("fly")
            .serve_in_process()
            .await
            .expect("agent");
        websockets
            .lock()
            .unwrap()
            .insert("fly".to_string(), agent.link());
        let err = service.save(row()).await.unwrap_err();
        assert!(err.contains("connected vendor process"), "{err}");
    }
}
