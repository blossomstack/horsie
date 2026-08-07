
import { McpServerFailure } from './mcpServerFailure';
import { PluginMcpTool } from './pluginMcpTool';
export interface McpDiscoverResponse {
  callId: string;
  tools: PluginMcpTool[];
  /**
   * Servers that contributed no tools. Reported rather than dropped, so a
   */
  failures: McpServerFailure[];
}