
/**
 * One forked conversation under a session.
 *
 * `parent` absent means the session's main agent; present names another fork.
 * That is what lets a client nest them to any depth without learning the
 * server's own vocabulary for where a fork is rooted.
 */
export interface ForkView {
  id: string;
  parent?: string;
  /**
   * What the fork named itself, once it has. Absent until then — a client
   * shows what it was branched from instead of inventing a name.
   */
  title?: string;
  /**
   * The same vocabulary an agent's document speaks: "provisioning" |
   * "running" | "idle" | "awaiting_input" | "failed" | "cancelled". A fork
   * is a conversation, so it never reads "completed".
   */
  status: string;
  createdAtMs: number;
}