use serde_json::Value;

/// One tool advertised by an MCP server (`tools/list`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    /// The tool's JSON Schema (`inputSchema`), passed through to the LLM as-is.
    pub input_schema: Value,
}

/// The outcome of a `tools/call`: the joined text content and whether the
/// server flagged it as an error (`isError`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallOutcome {
    pub is_error: bool,
    pub text: String,
}
