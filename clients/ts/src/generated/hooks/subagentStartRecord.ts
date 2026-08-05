
import { SubagentStartOutcome } from './subagentStartOutcome';
export interface SubagentStartRecord {
  agentId: string;
  agentType: string;
  systemMessage?: string;
  outcome: SubagentStartOutcome;
}