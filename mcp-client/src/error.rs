use thiserror::Error;

/// Everything that can go wrong talking to a remote MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// The HTTP request itself failed (connect/timeout/non-2xx status).
    #[error("transport error: {0}")]
    Transport(String),

    /// A well-formed HTTP response that isn't a usable JSON-RPC message
    /// (bad JSON, missing `result`, no response event in an SSE stream).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server returned a JSON-RPC `error` object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
}
