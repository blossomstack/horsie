
/**
 * How horsie authenticates to a remote MCP server. Informational mirror of the
 * `McpAuthView`/`McpAuthInput` tags.
 */
export enum McpAuthKind {
  None = "None",
  Bearer = "Bearer",
  OAuth = "OAuth",
  GithubApp = "GithubApp",
}