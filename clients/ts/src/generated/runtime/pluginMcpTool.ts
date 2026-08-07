
/**
 * One tool a plugin-declared MCP server offers. `name` is already namespaced
 */
export interface PluginMcpTool {
  name: string;
  description?: string;
  /**
   * JSON Schema, verbatim from the server.
   */
  inputSchema: string;
}