//! A minimal client for remote [MCP](https://modelcontextprotocol.io/) servers.
//!
//! The client speaks JSON-RPC over the MCP **Streamable HTTP** transport behind
//! a [`McpTransport`] seam, so the protocol logic ([`McpClient`]) is unit-tested
//! against a mock and the live HTTP path ([`HttpTransport`]) is swappable.
//!
//! horsie is the MCP *client*: this runs in the server process, next to the
//! agent loop, and never inside the sandbox.

mod client;
mod error;
mod transport;
mod types;

pub use client::McpClient;
pub use error::McpError;
pub use transport::{BearerProvider, HttpTransport, McpTransport};
pub use types::{McpCallOutcome, McpToolDef};
