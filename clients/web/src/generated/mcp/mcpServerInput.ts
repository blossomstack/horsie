
import { McpAuthInput } from './mcpAuthInput';
/**
 * Upsert input for a server. Secrets follow omit=keep, ""=clear semantics.
 */
export interface McpServerInput {
  name: string;
  url: string;
  /**
   * What this server is for, in your own words. Follows the same
   * omit=keep, ""=clear convention as the secrets: omitting it on an edit
   * keeps what is stored. Leave it clear to fall back to whatever the server
   * says about itself.
   */
  description?: string;
  auth: McpAuthInput;
}