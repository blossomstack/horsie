use serde_json::Value;

/// One tool advertised by an MCP server (`tools/list`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    /// The tool's JSON Schema (`inputSchema`), passed through to the LLM as-is.
    pub input_schema: Value,
}

/// One `image` block of a `tools/call` result, already base64-decoded.
///
/// The bytes and nothing else. The block's `mimeType` is deliberately dropped:
/// it is the server's *claim* about content the caller is holding, and whoever
/// stores these sniffs the type from the bytes. Carrying the claim would only
/// offer something to trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpImage(pub Vec<u8>);

/// The outcome of a `tools/call`: the joined text content, the images the call
/// produced, and whether the server flagged it as an error (`isError`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallOutcome {
    pub is_error: bool,
    pub text: String,
    /// Non-text blocks, in the order the server returned them. A screenshot
    /// tool answers with these and no text at all, which is why dropping them
    /// made such a call reach the model as an empty string.
    pub images: Vec<McpImage>,
}

/// What a server said about itself in the `initialize` handshake.
///
/// Every field is optional because every field is optional on the wire: a
/// server that offers nothing but tools is well within spec, and a handshake
/// must never fail over a missing pleasantry. `title` falls back to the
/// required `serverInfo.name` — displaying a blank where a name was available
/// is worse than displaying the id twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerInfo {
    pub title: Option<String>,
    pub version: Option<String>,
    /// The server's own guidance for a client (`instructions`). Stored and
    /// shown; not yet fed to the model.
    pub instructions: Option<String>,
}
