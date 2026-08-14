
/**
 * One call to a tool that ended the run by returning `StopRun`. Nothing is
 * recorded for it — no result, no completion event — so the `tool_use` stays
 * dangling, which is what lets an answer arrive against it later, or never
 */
export interface StoppedCall {
  /**
   * Which tool ended the run. The caller decides what that means; the agent
   * loop only reports it
   */
  tool: string;
  toolCallId: string;
  input: unknown;
}