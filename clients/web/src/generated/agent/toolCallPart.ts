
/**
 * A tool call requested by the model
 */
export interface ToolCallPart {
  id: string;
  name: string;
  input: unknown;
}