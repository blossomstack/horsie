
import { UserPromptExpansionOutcome } from './userPromptExpansionOutcome';
/**
 * `command` is the matcher domain: the name being expanded, so a hook can
 */
export interface UserPromptExpansionRecord {
  command: string;
  systemMessage?: string;
  outcome: UserPromptExpansionOutcome;
}