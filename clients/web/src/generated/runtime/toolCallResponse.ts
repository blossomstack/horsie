
import { HookRecord } from '../hooks';
import { ToolResult } from './toolResult';
export interface ToolCallResponse {
  callId: string;
  result: ToolResult;
  /**
   * Every hook that ran for this call, in execution order. Empty for the
   * overwhelmingly common case of a session with no matching hooks.
   */
  hooks: HookRecord[];
}