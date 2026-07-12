
import { McpAuthView } from './mcpAuthView';
/**
 * One configured remote MCP server, redacted for display.
 */
export interface McpServerView {
  /**
   * Stable id and namespace prefix for its tools (`mcp__&lt;name&gt;__&lt;tool&gt;`).
   */
  name: string;
  /**
   * Streamable-HTTP endpoint.
   */
  url: string;
  /**
   * Whether the last connect/test succeeded and the server is usable.
   */
  enabled: boolean;
  auth: McpAuthView;
  /**
   * Tools advertised at the last successful test, for the UI.
   */
  toolCount?: number;
  /**
   * Last connect/test error, for the UI.
   */
  lastError?: string;
}