
import { ToolCall } from './toolCall';
export interface ToolCallRequest {
  callId: string;
  agentId: string;
  call: ToolCall;
}