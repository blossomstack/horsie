
/**
 * A subagent&#39;s turn ending. Carries the same pair `SubagentStart` does, so a
 */
export interface SubagentStopInput {
  agentId: string;
  agentType: string;
  lastAssistantMessage?: string;
  stopHookActive: boolean;
}