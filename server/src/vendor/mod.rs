//! The runtime vendor protocol layer.
//!
//! A [`RuntimeVendor`] provisions and manages execution sandboxes for sessions —
//! the agent loop always stays server-side; vendors only provide tool execution,
//! a workspace, and a lifecycle. Every user action on a session translates into
//! exactly one explicit vendor signal (`create` / `attach` / `stop` / `delete`),
//! never an implicit side effect.

mod local;
/// A signal-recording vendor for tests only — never compiled into a production
/// build. Available to this crate's own tests (`cfg(test)`) and to external test
/// crates that opt in via the `test-util` feature.
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
mod velos;

pub use local::{LocalDaemonRegistry, LocalDaemonVendor};
pub use velos::{VelosMutableSettings, VelosVendor, VelosVendorSettings};

use async_trait::async_trait;
use horsie_runtime_client::RuntimeClient;
use std::path::PathBuf;
use std::sync::Arc;

/// Where a session workspace comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceSource {
    /// User-supplied host directory. No vendor kind currently accepts this
    /// (the shared local vendor ignores the daemon's fixed directory
    /// instead; velos rejects it outright) — kept for a future vendor kind
    /// that can honor it.
    HostDir(PathBuf),
    /// The vendor allocates and owns the directory (local: under its
    /// workspace root; velos: inside the container).
    Managed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSpec {
    pub name: String,
    pub source: WorkspaceSource,
}

/// Everything a vendor needs to provision (or revive) a runtime for a session.
/// The capability file is written by the session layer before any vendor call —
/// it is the durable source of truth a stopped runtime is revived against.
///
/// Workspaces are requests, not resolved paths — the vendor allocates `Managed`
/// entries itself.
#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub workspaces: Vec<WorkspaceSpec>,
    pub provision: Vec<horsie_models::executor::ProvisionStep>,
    pub env: Vec<horsie_models::executor::EnvVar>,
    pub capabilities_file: PathBuf,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
}

/// A live runtime a vendor handed back: the tool-call transport plus the
/// lifecycle handle.
pub struct VendorRuntime {
    pub runtime_client: RuntimeClient,
    pub handle: Arc<dyn VendorRuntimeHandle>,
}

/// Lifecycle handle for one live runtime instance.
#[async_trait]
pub trait VendorRuntimeHandle: Send + Sync {
    /// Halt without destroying (stop-preserve). Idempotent; the runtime stays
    /// re-attachable via [`RuntimeVendor::attach`].
    async fn stop(&self);
}

#[async_trait]
pub trait RuntimeVendor: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Provision a brand-new runtime.
    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError>;

    /// Revive a preserved runtime (respawn / resume / restart as the vendor sees
    /// fit — a local process respawns against the preserved workspace).
    async fn attach(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError>;

    /// The owning session was deleted; the vendor decides the runtime's fate.
    /// Callable with no live handle (e.g. after a server restart).
    async fn delete(&self, runtime_id: &str);

    /// Base URL a runtime should GET plugin-bundle artifacts from, reachable
    /// from where the runtime executes (loopback for local; `advertise_host`
    /// for velos). `None` disables plugin provisioning for this vendor (e.g.
    /// the mock vendor), so `ensure_runtime` injects no plugin env.
    fn artifact_base_url(&self) -> Option<String> {
        None
    }

    /// Filesystem path (host or in-container) the runtime unpacks bundles into
    /// and scans as its plugins dir. `None` → the runtime does not materialize
    /// bundles for this vendor.
    fn plugins_dir_for(&self, _runtime_id: &str) -> Option<String> {
        None
    }

    /// Optional content-hash cache dir (local vendor) so repeated sessions skip
    /// re-fetching and re-unpacking identical bundles.
    fn plugins_cache_dir(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VendorError {
    #[error("provision failed: {0}")]
    Provision(String),
    #[error("attach failed: {0}")]
    Attach(String),
}
