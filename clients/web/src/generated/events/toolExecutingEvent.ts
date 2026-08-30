
/**
 * Tool execution — message_id is the tool-result message id ("result:<tool_call_id>")
 */
export interface ToolExecutingEvent {
  messageId: string;
  toolCallId: string;
}