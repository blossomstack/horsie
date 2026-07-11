
/**
 * The result of executing a tool call
 */
export interface ToolResultPart {
  toolCallId: string;
  output: string;
  isError: boolean;
}