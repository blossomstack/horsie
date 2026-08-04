
import { HookRecord } from './hookRecord';
import { ToolResult } from './toolResult';
export interface ToolCallResponse {
  callId: string;
  result: ToolResult;
  /**
   * Every hook that ran for this call, in execution order. Empty for the
   */
  hooks: HookRecord[];
}