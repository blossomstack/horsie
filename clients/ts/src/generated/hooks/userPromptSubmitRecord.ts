
import { UserPromptSubmitOutcome } from './userPromptSubmitOutcome';
/**
 * Injects context via raw stdout as well as `additionalContext`.
 */
export interface UserPromptSubmitRecord {
  systemMessage?: string;
  outcome: UserPromptSubmitOutcome;
}