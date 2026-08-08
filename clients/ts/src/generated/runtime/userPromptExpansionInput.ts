
/**
 * An invocation about to be expanded. `command` is the matcher domain — the
 * name, so a hook can guard `/deploy` rather than every prompt — and `kind`
 * says which of `command`, `skill` or `agent` it named.
 */
export interface UserPromptExpansionInput {
  prompt: string;
  command: string;
  kind: string;
}