
/**
 * A subagent&#39;s turn ending. Carries `agent_type` as well, because that is what
 */
export interface SubagentStopInput {
  agentType: string;
  lastAssistantMessage?: string;
  stopHookActive: boolean;
}