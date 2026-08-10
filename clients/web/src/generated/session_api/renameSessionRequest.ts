
/**
 * Rename a session. Single line, non-empty, at most 60 characters — the same
 * rule the agent's own title tool is held to.
 */
export interface RenameSessionRequest {
  name: string;
}