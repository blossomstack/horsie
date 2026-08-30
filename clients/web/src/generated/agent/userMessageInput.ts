
import { ArtifactRef } from './artifactRef';
import { SubAgentResultPart } from './subAgentResultPart';
/**
 * New user message — starts a new turn
 */
export interface UserMessageInput {
  id: string;
  /**
   * May be empty: a turn started purely by owed subagent results carries
   * nothing the person typed.
   */
  text: string;
  /**
   * Finished subagents' results delivered with this turn.
   */
  subagentResults: SubAgentResultPart[];
  /**
   * What the person attached. Defaulted for the same durability reason as
   * `ToolResultPart.artifacts` — this type is journaled.
   */
  artifacts: ArtifactRef[];
}