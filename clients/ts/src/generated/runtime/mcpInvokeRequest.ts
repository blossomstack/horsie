
/**
 * Call one tool on a plugin-declared MCP server. `tool` is the namespaced name
 * from discovery; the runtime splits it back into server and tool.
 */
export interface McpInvokeRequest {
  callId: string;
  tool: string;
  arguments: string;
}