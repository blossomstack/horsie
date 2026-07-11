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

pub use local::LocalProcessVendor;

use async_trait::async_trait;
use horsie_runtime_client::RuntimeClient;
use std::path::PathBuf;
use std::sync::Arc;

/// Everything a vendor needs to provision (or revive) a runtime for a session.
/// The capability file is written by the session layer before any vendor call —
/// it is the durable source of truth a stopped runtime is revived against.
#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub workspaces: Vec<horsie_models::Workspace>,
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
}

#[derive(Debug, thiserror::Error)]
pub enum VendorError {
    #[error("provision failed: {0}")]
    Provision(String),
    #[error("attach failed: {0}")]
    Attach(String),
}
