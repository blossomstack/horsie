
import { McpAuthView } from './mcpAuthView';
/**
 * One configured remote MCP server, redacted for display.
 *
 * Deliberately carries no tool list. A server with forty tools would put its
 * whole catalogue into every listing, including the one the control plane
 * hands to a model — see `McpServerDetail`, which is what a caller that wants
 * the tools asks for.
 */
export interface McpServerView {
  /**
   * Stable id and namespace prefix for its tools (`mcp__<name>__<tool>`).
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
   * What this server is for, as it should be shown: the description someone
   * typed, else what the server called itself in the handshake. Absent when
   * neither exists.
   */
  description?: string;
  /**
   * Only the typed description, so an edit form shows an empty box rather
   * than presenting the server's own words as something a person wrote.
   */
  userDescription?: string;
  /**
   * The server's `instructions` from the last successful connect: its own
   * guidance on how it expects to be used.
   */
  instructions?: string;
  /**
   * How many tools it advertised at the last successful connect. Absent
   * means it has never connected — which is not the same as offering none.
   */
  toolCount?: number;
  /**
   * Last connect/test error, for the UI.
   */
  lastError?: string;
}