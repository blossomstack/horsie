
import { McpServerNeedsAuth } from './mcpServerNeedsAuth';
import { McpServerUnreachable } from './mcpServerUnreachable';
/**
 * Why a declared server contributed no tools this pass. Typed rather than a
 * string because the two cases call for different answers: one is a broken
 * plugin to report, the other is a consent flow to offer.
 */
export type McpServerFailure =
  | { failure: "Unreachable"; value: McpServerUnreachable }
  | { failure: "NeedsAuth"; value: McpServerNeedsAuth };