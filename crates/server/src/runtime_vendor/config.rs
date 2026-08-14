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
use crate::runtime_vendor::{RuntimeVendor, RuntimeVendorError, WebsocketVendorTable};
use crate::sessions::spec::RuntimeVendorMap;
use horsie_runtime_host::ConnectedRuntimeRegistry;
use sqlx::Row;
use sqlx::any::AnyRow;
use std::sync::{Arc, PoisonError};

const COLS: &str = "name, kind, settings, credential, created_at, updated_at";

/// The path on this server that runtimes dial. Named in the error a callback
/// URL without one earns, since that is the whole of what is missing.
pub(super) const CONNECT_PATH: &str = "/api/runtime/connect";

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

/// The smallest machine Fly will build. Below it a save is refused here rather
/// than by a machine-create rejection in the first session — and unlike a
/// *ceiling*, this is Fly's own documented minimum rather than a guess at a
/// shape catalogue that changes without us.
const MIN_FLY_MEMORY_MB: u32 = 256;

/// Reject a configuration that cannot possibly work, at save time.
///
/// Everything here is answerable without leaving the process. What only the
/// substrate can answer — is this token good, does this app exist — is asked
/// separately, by [`RuntimeVendor::preflight`], which is why this function
/// stays offline and total.
///
/// The alternative is a vendor that saves cleanly and then fails every session
/// with a timeout, because a machine on Fly dialled a hostname that only
/// resolves on the server's own loopback. That failure surfaces minutes later,
/// in a session, attributed to nothing.
///
pub fn validate(settings: &StoredVendorSettings, credential: &str) -> Result<(), String> {
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
            if fly.memory_mb < MIN_FLY_MEMORY_MB {
                return Err(format!(
                    "a fly machine needs at least {MIN_FLY_MEMORY_MB} MB of memory"
                ));
            }
            if fly.volumes && fly.volume_size_gb == 0 {
                return Err("a volume needs a size".to_string());
            }
            validate_callback(&fly.callback_url)
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
            validate_callback(&velos.callback_url)
        }
    }
}

/// Check a callback URL a machine has to reach this server from outside.
///
/// Validates and never rewrites. An earlier version completed a bare origin
/// with [`CONNECT_PATH`] and trimmed surrounding whitespace, which is a helpful
/// thing for a form to do and a harmful thing for an API: anything that
/// declares configuration reads back a value it never wrote, and cannot tell
/// that from drift. The completion lives in the settings form instead.
pub fn validate_callback(url: &str) -> Result<(), String> {
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
    // A bare origin makes the machine dial the server root and collect a 404 it
    // cannot explain, so it is refused rather than completed — and the message
    // names the path, because that is the whole of what is missing.
    if path.is_empty() {
        return Err(format!(
            "the callback url must include the connect path, e.g. wss://horsie.example.com{CONNECT_PATH}"
        ));
    }
    Ok(())
}

/// Why a save is refused after asking the substrate, or `None` when the answer
/// was not about the configuration at all.
///
/// The whole distinction lives here, in one place, because getting it backwards
/// is expensive in both directions: treat a reachability failure as a verdict
/// and a cloud outage stops anyone editing a vendor; treat a rejection as a
/// blip and the check buys nothing.
async fn preflight_refusal(vendor: &dyn RuntimeVendor) -> Option<String> {
    match vendor.preflight().await {
        Ok(()) => None,
        // The substrate answered, and the answer was no. No retry changes that,
        // so the configuration is not stored.
        Err(e @ (RuntimeVendorError::Provision(_) | RuntimeVendorError::Gone(_))) => {
            Some(refusal_message(e))
        }
        // It could not be reached, which says nothing about the configuration.
        // Refusing to store a vendor because a cloud API is having an outage
        // would be a worse failure than storing one that might be wrong.
        Err(e @ RuntimeVendorError::Unavailable(_)) => {
            tracing::warn!(
                vendor = %vendor.name(),
                error = %e,
                "a runtime vendor was saved unproved: its substrate could not be reached"
            );
            None
        }
    }
}

/// A check's failure, as the person looking at the settings form reads it.
fn refusal_message(e: RuntimeVendorError) -> String {
    match e {
        // Already written for them by the vendor that produced it.
        RuntimeVendorError::Provision(m) | RuntimeVendorError::Gone(m) => m,
        RuntimeVendorError::Unavailable(m) => format!("could not reach the vendor: {m}"),
    }
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
    /// The map sessions select from, shared with dialled-in vendors.
    vendors: RuntimeVendorMap,
    /// Which names belong to a dialled-in agent. Consulted so a save cannot
    /// take a live agent's name out from under it.
    websockets: WebsocketVendorTable,
    connected: Arc<ConnectedRuntimeRegistry>,
    /// The Fly Machines API root every Fly vendor here is built against.
    ///
    /// A value rather than the constant because a save now *calls* this API,
    /// which makes it something a deployment has to be able to point: at Fly's
    /// internal endpoint from inside an organisation's network, or at a stub in
    /// a test that must not reach the internet to save a vendor.
    fly_api_base: String,
}

impl RuntimeVendorConfigService {
    #[must_use]
    pub fn new(
        store: RuntimeVendorStore,
        vendors: RuntimeVendorMap,
        websockets: WebsocketVendorTable,
        connected: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            store,
            vendors,
            websockets,
            connected,
            fly_api_base: crate::runtime_vendor::fly_api::DEFAULT_API_BASE.to_string(),
        }
    }

    /// Point Fly vendors at another API root — see [`Self::fly_api_base`].
    #[must_use]
    pub fn with_fly_api_base(mut self, base: String) -> Self {
        self.fly_api_base = base;
        self
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
        let settings = StoredVendorSettings::from_wire(input.settings);
        // The UI has always treated the kind as fixed once saved — changing it
        // in place leaves every session pointing at a name whose substrate
        // silently moved. The API did not, so a PUT with a different kind came
        // back 200 and did exactly that; the Terraform provider is the caller
        // most likely to send one.
        if let Some(row) = existing.as_ref()
            && row.settings.kind() != settings.kind()
        {
            return Err(VendorConfigError::Invalid(format!(
                "runtime vendor '{name}' is a {} vendor and cannot become a {} one —                  delete it and create the new one under its own name, so sessions                  naming it are not silently repointed",
                row.settings.kind(),
                settings.kind()
            )));
        }
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
            settings,
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

    /// Validate, prove against the substrate, store, and publish. The published
    /// vendor replaces any previous one under the same name, which is how an
    /// edited token takes effect.
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
        // Stored exactly as it arrived: `validate` no longer rewrites anything,
        // so there is nothing to fold back into the row.
        validate(&row.settings, &row.credential)?;
        let vendor = self.build(&row)?;
        // One cheap call before anything is written. Until this existed, a
        // token with a typo in it and an app that was never created both saved
        // cleanly and failed hours later, inside a session, as a machine-create
        // rejection nobody could attribute to the form that caused it.
        if let Some(refusal) = preflight_refusal(vendor.as_ref()).await {
            return Err(refusal);
        }
        self.store.upsert(&row).await?;
        self.vendors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(row.name.clone(), vendor);
        Ok(row)
    }

    /// Ask a stored vendor's substrate whether it is usable right now.
    ///
    /// `Ok(None)` for a name nothing is configured under — the caller's
    /// mistake, which the route turns into a 404. Every other outcome is a
    /// result rather than an error, including the substrate saying no: the
    /// question was answered, and the answer is what was asked for.
    ///
    /// A save already refuses a configuration the substrate rejects, so this
    /// exists for what a save cannot cover — a token revoked, an app deleted,
    /// or a vendor stored while Fly was down.
    pub async fn test_named(
        &self,
        name: &str,
    ) -> Result<Option<horsie_models::runtime_vendor::RuntimeVendorTestResult>, VendorConfigError>
    {
        use horsie_models::runtime_vendor::RuntimeVendorTestResult as TestResult;
        let Some(row) = self
            .store
            .get(name)
            .await
            .map_err(VendorConfigError::Internal)?
        else {
            return Ok(None);
        };
        // Built from the row rather than read out of the published map: the
        // stored configuration is what a restart would come up with, and it is
        // the thing an operator is asking about.
        let vendor = self.build(&row).map_err(VendorConfigError::Invalid)?;
        Ok(Some(match vendor.preflight().await {
            Ok(()) => TestResult {
                ok: true,
                error: None,
            },
            Err(e) => TestResult {
                ok: false,
                error: Some(refusal_message(e)),
            },
        }))
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
                )
                .with_base(self.fly_api_base.clone());
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
            callback_url: "wss://horsie.example.com/api/runtime/connect".to_string(),
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
            vendors,
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            Arc::new(ConnectedRuntimeRegistry::new()),
        )
        // Saving a fly vendor now calls the Machines API. Every test that is
        // not *about* that answer points it at a port nothing listens on, so
        // the check fails as "unreachable" — instantly, locally, and without a
        // verdict on the configuration.
        .with_fly_api_base(crate::testing::UNREACHABLE_FLY_API.to_string())
    }

    /// A stub Machines API that answers `GET /apps/{app}/machines` with
    /// `status`, and records what it was asked.
    async fn fly_stub(status: u16) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;

        type Seen = Arc<std::sync::Mutex<Vec<String>>>;
        async fn machines(
            State((status, seen)): State<(u16, Seen)>,
            Path(app): Path<String>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            seen.lock().unwrap().push(format!("{app} {auth}"));
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                axum::Json(serde_json::json!([])),
            )
        }

        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/apps/{app}/machines", axum::routing::get(machines))
            .with_state((status, seen.clone()));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    fn empty_map() -> RuntimeVendorMap {
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()))
    }

    #[test]
    fn a_bare_origin_is_refused() {
        // The server used to complete this silently. It cannot: a client that
        // declares configuration reads back a value it never wrote and has no
        // way to tell that from drift. Completing it is a typing affordance,
        // and belongs in the form.
        let err = validate_callback("wss://horsie.example.com").unwrap_err();
        assert!(
            err.contains("/api/runtime/connect"),
            "{err} must name the path it wants"
        );
    }

    #[test]
    fn a_trailing_slash_is_refused() {
        // Still no path — and completing it used to produce
        // `//api/runtime/connect`, which axum will not route.
        assert!(validate_callback("wss://horsie.example.com/").is_err());
    }

    #[test]
    fn an_explicit_path_is_accepted() {
        assert!(validate_callback("wss://horsie.example.com/relay/rt").is_ok());
        assert!(validate_callback("wss://horsie.example.com/api/runtime/connect").is_ok());
    }

    #[test]
    fn surrounding_whitespace_is_refused() {
        // Not trimmed, for the reason a bare origin is not completed: whatever
        // is stored has to be exactly what was sent.
        assert!(validate_callback(" wss://horsie.example.com/api/runtime/connect").is_err());
    }

    #[test]
    fn a_loopback_callback_is_refused() {
        // The whole point of validating at save time: this configuration saves
        // cleanly and then fails every session as an unexplained timeout. Each
        // url carries a path, so the refusal is about the host and nothing else.
        for url in [
            "ws://localhost:8080/api/runtime/connect",
            "ws://127.0.0.1:8080/api/runtime/connect",
            "wss://[::1]:8080/api/runtime/connect",
            "ws://0.0.0.0:8080/api/runtime/connect",
            "ws://app.localhost/api/runtime/connect",
        ] {
            assert!(validate_callback(url).is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn a_non_websocket_scheme_is_refused() {
        // The runtime's own endpoint parser accepts only ws/wss, so an https
        // URL would fail inside the machine where nobody can see it.
        assert!(validate_callback("https://horsie.example.com/api/runtime/connect").is_err());
        assert!(validate_callback("horsie.example.com/api/runtime/connect").is_err());
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
            // A port nothing listens on: saving this vendor asks velos who we
            // are, and a hostname would put a DNS lookup in the middle of a
            // unit test.
            server_url: "http://127.0.0.1:1".to_string(),
            image: "ghcr.io/x/runtime:1".to_string(),
            callback_url: "ws://horsie.internal:8080/api/runtime/connect".to_string(),
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

    // The UI has always treated the kind as fixed once saved; the API did not,
    // so a PUT with a different kind returned 200 and silently moved every
    // session naming that vendor onto a different substrate.
    #[tokio::test]
    async fn a_saved_vendors_kind_cannot_be_changed_through_the_api() {
        use horsie_models::runtime_vendor::{
            FlyVendorSettings, RuntimeVendorConfigInput, RuntimeVendorSettings, VelosVendorSettings,
        };
        let db = crate::db::testing::db().await;
        let service = service(empty_map(), db);
        let callback = "wss://horsie.example.com/api/runtime/connect".to_string();

        let fly = RuntimeVendorConfigInput {
            name: "vendor".to_string(),
            settings: RuntimeVendorSettings::Fly(FlyVendorSettings {
                app: "horsie-runtimes".to_string(),
                image: "ghcr.io/x/runtime:1".to_string(),
                region: "iad".to_string(),
                workspace_root: "/workspaces".to_string(),
                callback_url: callback.clone(),
                volumes: false,
                volume_size_gb: 1,
                cpu_kind: "shared".to_string(),
                cpus: 1,
                memory_mb: 512,
            }),
            credential: Some("fly-token".to_string()),
        };
        service.save_input("vendor", fly.clone()).await.unwrap();

        let velos = RuntimeVendorConfigInput {
            name: "vendor".to_string(),
            settings: RuntimeVendorSettings::Velos(VelosVendorSettings {
                server_url: "http://velos.example:8080".to_string(),
                image: "ghcr.io/x/runtime:1".to_string(),
                runtime_bin: "/usr/local/bin/horsie-runtime".to_string(),
                workspace_root: "/workspaces".to_string(),
                callback_url: callback,
                cpu: 1,
                memory_mb: 512,
            }),
            credential: Some(String::new()),
        };
        let err = service.save_input("vendor", velos).await.unwrap_err();
        assert!(
            matches!(&err, VendorConfigError::Invalid(m) if m.contains("cannot become")),
            "{err:?}"
        );

        // Re-saving the same kind is still how a rotated token is applied.
        assert!(service.save_input("vendor", fly).await.is_ok());
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

    /// The finding this whole path exists for: a token with a typo in it and an
    /// app that was never created both used to save cleanly, and both surfaced
    /// hours later inside a session as a machine-create rejection.
    #[tokio::test]
    async fn a_token_or_an_app_fly_rejects_is_refused_at_save_time() {
        let db = crate::db::testing::db().await;
        let (base, seen) = fly_stub(401).await;
        let vendors = empty_map();
        let saving = service(vendors.clone(), db.clone()).with_fly_api_base(base);

        let err = saving.save(row()).await.unwrap_err();
        assert!(
            err.contains("401") && err.contains("app already exists"),
            "the refusal has to say what to go and check: {err}"
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["horsie-runtimes Bearer fly-token".to_string()],
            "one listing, with the app in the path and the token on it — that is what proves both"
        );
        assert!(
            vendors.read().unwrap().is_empty(),
            "a refused vendor must not be published"
        );
        assert!(
            service(empty_map(), db).list().await.unwrap().is_empty(),
            "nor stored"
        );
    }

    #[tokio::test]
    async fn a_vendor_fly_accepts_is_saved() {
        let db = crate::db::testing::db().await;
        let (base, _seen) = fly_stub(200).await;
        let vendors = empty_map();
        service(vendors.clone(), db)
            .with_fly_api_base(base)
            .save(row())
            .await
            .unwrap();
        assert!(vendors.read().unwrap().contains_key("fly"));
    }

    #[tokio::test]
    async fn a_vendor_is_still_saved_when_fly_itself_is_down() {
        // The other half of the rule. A 5xx says nothing about this token or
        // this app, and an operator locked out of editing their vendors until
        // someone else's outage ends is a worse failure than an unproved row.
        let db = crate::db::testing::db().await;
        let (base, _seen) = fly_stub(503).await;
        let vendors = empty_map();
        service(vendors.clone(), db)
            .with_fly_api_base(base)
            .save(row())
            .await
            .unwrap();
        assert!(vendors.read().unwrap().contains_key("fly"));
    }

    #[tokio::test]
    async fn boot_publishes_a_stored_vendor_without_asking_fly_anything() {
        // Republishing is not the moment to re-litigate a configuration: a
        // vendor that failed a check at boot would take its sessions away over
        // an outage, and every restart would pay a round trip per vendor.
        let db = crate::db::testing::db().await;
        let (base, seen) = fly_stub(200).await;
        service(empty_map(), db.clone())
            .with_fly_api_base(base.clone())
            .save(row())
            .await
            .unwrap();
        seen.lock().unwrap().clear();

        let vendors = empty_map();
        service(vendors.clone(), db)
            .with_fly_api_base(base)
            .publish_all()
            .await;
        assert!(vendors.read().unwrap().contains_key("fly"));
        assert!(
            seen.lock().unwrap().is_empty(),
            "publish_all called fly: {:?}",
            seen.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn testing_a_vendor_reports_what_the_substrate_said() {
        let db = crate::db::testing::db().await;
        let (ok_base, _) = fly_stub(200).await;
        let live = service(empty_map(), db.clone()).with_fly_api_base(ok_base);
        live.save(row()).await.unwrap();
        let result = live.test_named("fly").await.unwrap().unwrap();
        assert!(result.ok && result.error.is_none(), "{result:?}");

        // The case a save cannot cover: the same stored row, a token the
        // substrate has since stopped accepting.
        let (bad_base, _) = fly_stub(401).await;
        let stale = service(empty_map(), db).with_fly_api_base(bad_base);
        let result = stale.test_named("fly").await.unwrap().unwrap();
        assert!(!result.ok, "{result:?}");
        assert!(
            result.error.unwrap_or_default().contains("401"),
            "the substrate's own answer is what the operator needs"
        );
    }

    #[tokio::test]
    async fn testing_a_vendor_that_does_not_exist_is_not_a_failed_test() {
        // Absent is not "unreachable": the route answers 404 for it, and
        // reporting `ok: false` would have a name nobody configured look like a
        // vendor with a bad token.
        let db = crate::db::testing::db().await;
        assert!(
            service(empty_map(), db)
                .test_named("nobody")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_machine_too_small_to_boot_is_refused() {
        // Not covered by the listing check — machine size only rides a create —
        // and 256 MB is fly's own floor rather than a guess at a catalogue that
        // changes without us. There is deliberately no ceiling for the same
        // reason: any number here would eventually refuse a shape fly is happy
        // to build.
        let StoredVendorSettings::Fly(fly) = settings() else {
            panic!("the fixture is a fly vendor")
        };
        let err = validate(
            &StoredVendorSettings::Fly(StoredFlySettings {
                memory_mb: 1,
                ..fly.clone()
            }),
            "t",
        )
        .unwrap_err();
        assert!(err.contains("256"), "{err}");
        assert!(
            validate(
                &StoredVendorSettings::Fly(StoredFlySettings {
                    memory_mb: 256,
                    cpus: 999,
                    ..fly
                }),
                "t"
            )
            .is_ok(),
            "an unusual cpu count is fly's call to make, not ours"
        );
    }

    #[tokio::test]
    async fn a_save_cannot_steal_a_connected_agents_name() {
        let db = crate::db::testing::db().await;
        let websockets: WebsocketVendorTable =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let service = RuntimeVendorConfigService::new(
            RuntimeVendorStore::new(db, UserId::new("u1")),
            empty_map(),
            websockets.clone(),
            Arc::new(ConnectedRuntimeRegistry::new()),
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
