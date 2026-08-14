
/**
 * Call one tool on a plugin-declared MCP server. `tool` is the namespaced name
 * from discovery; the runtime splits it back into server and tool.
 * `agent_id` names whose tree hosts the server. The same id the discovery that
 * produced this tool name carried — a tool discovered in one agent's bundles
 * cannot be invoked against another's.
 */
export interface McpInvokeRequest {
  callId: string;
  agentId: string;
  tool: string;
  arguments: string;
}