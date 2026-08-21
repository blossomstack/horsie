
/**
 * Whether a tool observes or changes something.
 *
 * A judgement about the tool, not about one call: `bash` is `Write` because it
 * *can* write, and `horsie_sessions` is `Write` because one of its actions
 * creates a session. Anything that could go either way is `Write` — the point
 * of the distinction is to make "read-only" mean it.
 */
export enum ToolAccess {
  Read = "Read",
  Write = "Write",
}