//! A minimal client for remote [MCP](https://modelcontextprotocol.io/) servers.
//!
//! The client speaks JSON-RPC over the MCP **Streamable HTTP** transport behind
//! a [`McpTransport`] seam, so the protocol logic ([`McpClient`]) is unit-tested
//! against a mock and the live HTTP path ([`HttpTransport`]) is swappable.
//!
//! horsie is the MCP *client* in two places. Admin-configured servers are
//! reached from the server process, next to the agent loop. Plugin-declared
//! servers are reached from the **runtime**, because a plugin's `npx …` server
//! is a process that belongs next to the workspace — see [`StdioTransport`].
//! Both share [`McpClient`]; only the framing differs.

mod client;
mod error;
mod stdio;
mod transport;
mod types;

pub use client::McpClient;
pub use error::McpError;
pub use stdio::StdioTransport;
pub use transport::{BearerProvider, HttpTransport, McpTransport};
pub use types::{McpCallOutcome, McpImage, McpServerInfo, McpToolDef};
