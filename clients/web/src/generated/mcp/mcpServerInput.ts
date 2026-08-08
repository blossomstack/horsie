
import { McpAuthInput } from './mcpAuthInput';
/**
 * Upsert input for a server. Secrets follow omit=keep, ""=clear semantics.
 */
export interface McpServerInput {
  name: string;
  url: string;
  auth: McpAuthInput;
}