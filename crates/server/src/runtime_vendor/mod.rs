//! The runtime vendor layer.
//!
//! A runtime vendor owns runtime lifecycle; the agent loop always stays
//! server-side, and vendors only provide tool execution, a workspace, and a
//! lifecycle. Every user action on a session translates into exactly one
//! explicit vendor signal, never an implicit side effect.
//!
//! The contract itself — [`RuntimeVendor`] and [`RuntimeHandle`] — lives in
//! `horsie-runtime-vendor`, because the same two traits describe both sides of
//! the wire: this server drives a [`WebsocketRuntimeVendor`] that relays to a
//! `horsie connect` process, and that process drives a vendor of its own.
//!
//! [`WebsocketRuntimeVendor`] is one implementation, not the only shape a vendor
//! can have. An earlier revision deleted the trait as "pure indirection" when
//! every vendor was a socket; it stops being indirection the moment one is not.

pub mod config;
/// A scriptable runtime vendor for tests only — never compiled into a
/// production build. Available to this crate's own tests (`cfg(test)`) and to
/// external test crates that opt in via the `test-util` feature.
#[cfg(any(test, feature = "test-util"))]
pub mod fake;
pub mod fly;
pub mod fly_api;
mod registry;
mod transport;
mod websocket;

pub use config::{
    FlyVendorSettings, RuntimeVendorConfigService, RuntimeVendorRow, RuntimeVendorSettings,
    RuntimeVendorStore,
};
pub use horsie_models::runtime_vendor::RuntimeVendorCapabilities;
pub use horsie_runtime_vendor::runtime_vendor::{RuntimeHandle, RuntimeVendor};
pub use horsie_runtime_vendor::{RuntimeProgress, RuntimeVendorError};
pub use registry::{RegisterError, RuntimeVendorRegistry, WebsocketVendorTable};
pub use transport::RuntimeVendorTransport;
pub use websocket::WebsocketRuntimeVendor;

/// A session workspace request. The directory is always vendor-allocated
/// (velos: inside the container; local: the connected daemon's own dir), so a
/// workspace is just a name the vendor maps to a path it owns.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSpec {
    pub name: String,
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

impl RuntimeSpec {
    /// The wire shape the vendor contract speaks.
    ///
    /// The server models a workspace as a named request and the wire as a bare
    /// name, so this is where the two meet. Kept as a conversion rather than
    /// collapsing the types because a workspace request is due to grow fields
    /// (size, retention) that mean nothing on the wire.
    #[must_use]
    pub fn to_wire(&self) -> horsie_models::runtime_vendor::RuntimeSpec {
        horsie_models::runtime_vendor::RuntimeSpec {
            workspaces: self.workspaces.iter().map(|w| w.name.clone()).collect(),
            env: self.env.clone(),
            provision: self.provision.clone(),
        }
    }
}
