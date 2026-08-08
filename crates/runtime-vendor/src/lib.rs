mod baseline;
mod connected_registry;
mod env_scrub;
mod error;
mod listener;
mod process_provider;
mod provider;
mod reconnect;
mod runtime_listener;
/// The vendor contract. A public module rather than a root re-export while the
/// old `provider::RuntimeHandle` still exists: two traits of that name would be
/// a genuine ambiguity for a reader, and the old one is deleted as each vendor
/// is ported onto this one.
pub mod runtime_vendor;
mod socket_transport;
mod vendor;

pub use connected_registry::ConnectedRuntimeRegistry;
pub use env_scrub::{SANDBOX_ENV_ALLOWLIST, scrubbed_env};
pub use error::{CredentialError, ExecutorError, RuntimeError};
pub use listener::{handle_runtime_connection, serve_runtime_connections};
pub use process_provider::{ProcessRuntimeProvider, SandboxPolicy};
pub use provider::{HealthStatus, RuntimeHandle, RuntimeProvider};
pub use reconnect::Backoff;
pub use runtime_listener::{AcceptedStream, RuntimeEndpoint, RuntimeListenerServer};
pub use runtime_vendor::{
    RuntimeEvent, RuntimeHandleImpl, RuntimeHandleTransport, RuntimeProgress, RuntimeProgressSink,
    RuntimeVendorError, new_dial_secret,
};
pub use socket_transport::{SocketRuntimeTransport, UnixSocketRuntimeTransport};
pub use vendor::{
    AgentExit, BundleDelivery, CredentialProvider, FixedWorkspaces, ProviderFactory,
    RuntimeVendorClient, WorkspaceResolver, no_credential,
};
