
/**
 * One tool a plugin-declared MCP server offers. `name` is already namespaced
 * `mcp__<server>__<tool>`, the spelling admin-configured servers use — so
 * `allowed_tools` and hook matchers see one vocabulary whichever path a tool
 * came from.
 */
export interface PluginMcpTool {
  name: string;
  description?: string;
  /**
   * JSON Schema, verbatim from the server.
   */
  inputSchema: string;
}