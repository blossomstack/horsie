//! Configured remote MCP servers the session agent calls server-side: a
//! A database-backed store, and a service that builds MCP clients with the right
//! auth (stored bearer, or a user token reused from the GitHub App connection),
//! runs the connect/smoke test, and hands the agent per-session toolboxes.

mod oauth;
mod service;
mod store;

pub mod selection;

pub use service::McpService;
pub use store::{ConnectOutcome, McpServerRow, McpStore, StoredAuth};
