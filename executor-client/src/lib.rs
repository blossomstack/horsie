//! Lifecycle client to a connected executor, plus the transports that back it.
//!
//! Split out from `server` so callers (the CLI, the executor itself) can drive
//! runtime lifecycle without depending on the WS `Server`. Tool calls do *not* go
//! through this client — they use a [`RuntimeClient`](runtime_client::RuntimeClient)
//! obtained from [`ExecutorClient::runtime_transport`].

mod client;
mod transport;
#[cfg(feature = "ws")]
mod ws_transport;

pub use client::{ClientError, ExecutorClient};
pub use transport::ExecutorTransport;
#[cfg(feature = "ws")]
pub use ws_transport::WsExecutorTransport;
