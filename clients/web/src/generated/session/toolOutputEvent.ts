
/**
 * `at_ms` is the unix-epoch millisecond the tool finished — the same stamp the
 */
export interface ToolOutputEvent {
  toolCallId: string;
  output: string;
  isError: boolean;
  atMs: number;
}