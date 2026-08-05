
import { SubagentStopOutcome } from './subagentStopOutcome';
export interface SubagentStopRecord {
  agentType: string;
  systemMessage?: string;
  outcome: SubagentStopOutcome;
}