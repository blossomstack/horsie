
/**
 * Connect to every MCP server the loaded plugins declare and list their tools.
 *
 * One request rather than one per server: a session wants the whole tool list
 * or none of it, and a server that cannot start contributes nothing rather than
 * failing the scan.
 */
export interface McpDiscoverRequest {
  callId: string;
}