
import { ToolResultInput } from './toolResultInput';
import { UserMessageInput } from './userMessageInput';
/**
 * The only valid inputs to Agent::run
 */
export type AgentInput =
  | { type: "UserMessage"; value: UserMessageInput }
  | { type: "ToolResult"; value: ToolResultInput };