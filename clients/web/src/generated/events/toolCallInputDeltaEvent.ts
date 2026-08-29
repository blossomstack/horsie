
export interface ToolCallInputDeltaEvent {
  messageId: string;
  index: number;
  toolCallId: string;
  delta: string;
}