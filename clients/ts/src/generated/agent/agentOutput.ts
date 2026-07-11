
import { AgentResult } from './agentResult';
import { Usage } from './usage';
/**
 * The full output of Agent::run
 */
export interface AgentOutput {
  result: AgentResult;
  usage: Usage;
}