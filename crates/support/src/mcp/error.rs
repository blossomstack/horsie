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

    /// The server refused the request for want of credentials.
    ///
    /// Distinct from [`McpError::Transport`] because it is the one failure a
    /// caller can act on: `www_authenticate` is the RFC 9728 challenge naming
    /// where to go and get a token, and collapsing it into a string throws away
    /// the whole discovery mechanism.
    #[error("unauthorized")]
    Unauthorized { www_authenticate: Option<String> },

    /// The server returned a JSON-RPC `error` object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
}
