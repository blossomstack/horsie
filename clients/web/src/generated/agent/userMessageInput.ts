
import { SubAgentResultPart } from './subAgentResultPart';
/**
 * New user message — starts a new turn
 */
export interface UserMessageInput {
  id: string;
  /**
   * May be empty: a turn started purely by owed subagent results carries
   */
  text: string;
  /**
   * Finished subagents&#39; results delivered with this turn.
   */
  subagentResults: SubAgentResultPart[];
}