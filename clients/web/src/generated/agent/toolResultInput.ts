
import { ArtifactRef } from './artifactRef';
/**
 * Tool result input — resumes the agent after a handoff
 */
export interface ToolResultInput {
  toolCallId: string;
  output: string;
  isError: boolean;
  /**
   * Defaulted for the same durability reason as `ToolResultPart`.
   */
  artifacts: ArtifactRef[];
}