
import { PluginMcpTool } from './pluginMcpTool';
export interface McpDiscoverResponse {
  callId: string;
  tools: PluginMcpTool[];
  /**
   * Servers that could not be reached, as `&#60;name&#62;: &#60;why&#62;`. Reported rather
   */
  failures: string[];
}