
import { McpServerNeedsAuth } from './mcpServerNeedsAuth';
import { McpServerUnreachable } from './mcpServerUnreachable';
/**
 * Why a declared server contributed no tools this pass. Typed rather than a
 */
export type McpServerFailure =
  | { failure: "Unreachable"; value: McpServerUnreachable }
  | { failure: "NeedsAuth"; value: McpServerNeedsAuth };