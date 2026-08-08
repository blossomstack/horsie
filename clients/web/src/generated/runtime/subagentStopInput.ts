
/**
 * A subagent's turn ending. Carries the same pair `SubagentStart` does, so a
 * hook can join its own start to its own stop.
 */
export interface SubagentStopInput {
  agentId: string;
  agentType: string;
  lastAssistantMessage?: string;
  stopHookActive: boolean;
}