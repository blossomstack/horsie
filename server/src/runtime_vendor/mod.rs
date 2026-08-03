//! The runtime vendor layer.
//!
//! A vendor is an external agent process that owns runtime lifecycle; the agent
//! loop always stays server-side, and vendors only provide tool execution, a
//! workspace, and a lifecycle. Every user action on a session translates into
//! exactly one explicit vendor signal (`create` / `attach` / `stop` / `delete`),
//! never an implicit side effect.
//!
//! There is one vendor type — [`RuntimeVendorLink`], the server's end of a connected
//! agent's WebSocket. The `RuntimeVendor` trait this module used to define was
//! pure indirection once the in-process vendors were deleted.

/// A scriptable vendor agent for tests only — never compiled into a production
/// build. Available to this crate's own tests (`cfg(test)`) and to external test
/// crates that opt in via the `test-util` feature.
#[cfg(any(test, feature = "test-util"))]
pub mod fake;
mod link;
mod registry;
mod transport;

pub use link::RuntimeVendorLink;
pub use registry::{RegisterError, RuntimeVendorRegistry};
pub use transport::RuntimeVendorTransport;

use async_trait::async_trait;
use horsie_runtime_client::RuntimeClient;
use std::sync::Arc;

/// A session workspace request. The directory is always vendor-allocated
/// (velos: inside the container; local: the connected daemon's own dir), so a
/// workspace is just a name the vendor maps to a path it owns.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSpec {
    pub name: String,
}

/// What a vendor can do with a session's workspace, announced by the vendor
/// itself so the server and UI never branch on vendor name/kind. Extensible:
/// add a field here and each vendor declares its own value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorCapabilities {
    /// The vendor provisions a fresh workspace it owns — cloning repos,
    /// installing skill bundles, running provision steps. A vendor that runs
    /// in a fixed, user-owned directory (e.g. the shared local daemon)
    /// provisions nothing and announces `false`.
    pub supports_provisioning: bool,
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
}

/// A live runtime a vendor handed back: the tool-call transport plus the
/// lifecycle handle.
pub struct VendorRuntime {
    pub runtime_client: RuntimeClient,
    pub handle: Arc<dyn VendorRuntimeHandle>,
}

impl std::fmt::Debug for VendorRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VendorRuntime")
            .field("runtime_client", &"<RuntimeClient>")
            .field("handle", &"<dyn VendorRuntimeHandle>")
            .finish()
    }
}

/// Lifecycle handle for one live runtime instance.
#[async_trait]
pub trait VendorRuntimeHandle: Send + Sync {
    /// Advisory suspend. Idempotent; the runtime stays reachable via
    /// [`RuntimeVendorLink::get`] afterwards, whether the vendor actually
    /// suspended anything or kept it running.
    async fn hibernate(&self);
}

#[derive(Debug, thiserror::Error)]
pub enum VendorError {
    /// A create could not provision the runtime. The session can try again.
    #[error("provision failed: {0}")]
    Provision(String),
    /// A live vendor has no runtime under this id and cannot produce one.
    /// Terminal for the session — the alternative would be silently rebuilding
    /// a workspace the user believes still exists.
    #[error("runtime is gone: {0}")]
    Gone(String),
    /// The vendor itself is unreachable: not registered, or its socket is
    /// dead. Always retryable, and never to be confused with [`Self::Gone`].
    #[error("vendor unavailable: {0}")]
    Unavailable(String),
}
