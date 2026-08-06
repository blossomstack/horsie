
import { McpServerNeedsAuth } from './mcpServerNeedsAuth';
import { ToolResult } from './toolResult';
export interface McpInvokeResponse {
  callId: string;
  result: ToolResult;
  /**
   * Set when the call failed because the server refused the credential.
   */
  needsAuth?: McpServerNeedsAuth;
}