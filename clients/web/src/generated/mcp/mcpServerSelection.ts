
/**
 * One MCP server a session or preset may call, and how much of it.
 *
 * `tools` absent means **every tool this server offers, now and later** — so a
 * server that gains a tool reaches an existing preset without anyone editing
 * it. A list narrows the selection to those names; a tool not in it is neither
 * offered to the model nor callable. An empty list is the same as not
 * selecting the server at all, and the UI does not produce one.
 *
 * Names are the server's own, unprefixed: `search_issues`, not
 * `mcp__linear__search_issues`. The prefix belongs to the tool id a model
 * sees, and repeating the server name inside its own selection would be one
 * more thing that can disagree with the field beside it.
 */
export interface McpServerSelection {
  name: string;
  tools?: string[];
}