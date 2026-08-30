
export interface ToolCallStartEvent {
  messageId: string;
  index: number;
  toolCallId: string;
  name: string;
}