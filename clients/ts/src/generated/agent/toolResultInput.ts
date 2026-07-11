
/**
 * Tool result input — resumes the agent after a handoff
 */
export interface ToolResultInput {
  toolCallId: string;
  output: string;
  isError: boolean;
}