//! Configured remote MCP servers the session agent calls server-side: a
//! SQLite-backed store, and a service that builds MCP clients with the right
//! auth (stored bearer, or a user token reused from the GitHub App connection),
//! runs the connect/smoke test, and hands the agent per-session toolboxes.

mod service;
mod store;

pub use service::McpService;
pub use store::{McpServerRow, McpStore, StoredAuth};
