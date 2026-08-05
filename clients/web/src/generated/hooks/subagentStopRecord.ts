
import { SubagentStopOutcome } from './subagentStopOutcome';
export interface SubagentStopRecord {
  agentId: string;
  agentType: string;
  systemMessage?: string;
  outcome: SubagentStopOutcome;
}