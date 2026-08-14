//! The host side of the runtime wire.
//!
//! Two halves of one protocol: dialling into a runtime ([`RuntimeClient`],
//! [`RuntimeTransport`]) and supplying one ([`RuntimeVendorClient`], the
//! listener, the credential provider). The sandboxed child process at the far
//! end of that wire is `horsie-runtime`.

mod baseline;
mod client;
mod connected_registry;
mod env_scrub;
mod error;
mod in_flight;
mod issued_tokens;
mod listener;
mod process_provider;
mod provider;
mod reconnect;
mod runtime_listener;
/// The vendor contract. A public module rather than a root re-export while the
/// old `provider::RuntimeHandle` still exists: a root export of both would be a
/// genuine ambiguity for a reader, and the old one is deleted as each vendor is
/// ported onto this one.
pub mod runtime_vendor;
mod socket_transport;
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
pub mod tools;
mod transport;
mod vendor;

pub use baseline::baseline_capabilities;
pub use client::{HookSink, RuntimeCallError, RuntimeClient};
pub use connected_registry::ConnectedRuntimeRegistry;
pub use env_scrub::{SANDBOX_ENV_ALLOWLIST, scrubbed_env};
pub use error::{CredentialError, ExecutorError, RuntimeError};
pub use in_flight::InFlight;
pub use issued_tokens::IssuedTokens;
pub use listener::{handle_runtime_connection, serve_runtime_connections};
pub use process_provider::{ProcessRuntimeProvider, SandboxPolicy};
pub use provider::{HealthStatus, RuntimeHandle, RuntimeProvider};
pub use reconnect::Backoff;
pub use runtime_listener::{AcceptedStream, RuntimeEndpoint, RuntimeListenerServer};
pub use runtime_vendor::{RuntimeEvent, RuntimeProgress, RuntimeProgressSink, RuntimeVendorError};
pub use socket_transport::{SocketRuntimeTransport, UnixSocketRuntimeTransport};
#[cfg(any(test, feature = "test-util"))]
pub use testkit::{BlockHandle, MockTransport, TransportOutcome, TransportProbe};
pub use tools::add_runtime_tools;
pub use transport::{
    RuntimeTransport, TransportError, closed_when, inbound_call_id, outbound_call_id,
};
pub use vendor::{
    AgentExit, BundleDelivery, CredentialProvider, FixedWorkspaces, ProviderFactory,
    RuntimeVendorClient, WorkspaceResolver, no_credential,
};
