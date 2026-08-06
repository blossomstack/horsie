
/**
 * An invocation about to be expanded. `command` is the matcher domain — the
 */
export interface UserPromptExpansionInput {
  prompt: string;
  command: string;
  kind: string;
}