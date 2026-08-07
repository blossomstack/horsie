
/**
 * Call one tool on a plugin-declared MCP server. `tool` is the namespaced name
 */
export interface McpInvokeRequest {
  callId: string;
  tool: string;
  arguments: string;
}