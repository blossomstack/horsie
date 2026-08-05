
import { SubagentStartOutcome } from './subagentStartOutcome';
export interface SubagentStartRecord {
  agentType: string;
  systemMessage?: string;
  outcome: SubagentStartOutcome;
}