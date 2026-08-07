
import { ToolResult } from './toolResult';
export interface McpInvokeResponse {
  callId: string;
  result: ToolResult;
}