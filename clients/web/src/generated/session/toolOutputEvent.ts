
export interface ToolOutputEvent {
  toolCallId: string;
  output: string;
  isError: boolean;
}