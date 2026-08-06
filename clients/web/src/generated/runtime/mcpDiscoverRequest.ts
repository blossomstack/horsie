
import { McpCredential } from './mcpCredential';
export interface McpDiscoverRequest {
  callId: string;
  credentials: McpCredential[];
}