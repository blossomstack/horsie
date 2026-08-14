
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
  /**
   * When this fork last did anything: the moment of its most recent status
   * change, which is the end of its last turn once it is idle again.
   *
   * A conversation has no *end* — nothing closes it — so this is not one.
   * It is how far along the fork got, which is what a reader looking at the
   * session's shape actually needs; without it a fork can only be drawn as
   * running forever. Zero before the fork has moved at all.
   */
  lastActivityMs: number;
}