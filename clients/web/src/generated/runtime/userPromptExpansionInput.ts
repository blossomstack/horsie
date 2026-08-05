
/**
 * A slash command about to be expanded. `command` is the matcher domain.
 */
export interface UserPromptExpansionInput {
  prompt: string;
  command: string;
}