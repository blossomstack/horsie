
/**
 * One sub session under a session.
 *
 * `parent` absent means the session's main agent; present names another sub
 * session. That is what lets a client nest them to any depth without learning
 * the server's own vocabulary for where one is rooted.
 */
export interface SubSessionView {
  id: string;
  parent?: string;
  /**
   * What the sub session named itself, once it has. Absent until then — a
   * client shows what it was branched from instead of inventing a name.
   */
  title?: string;
  /**
   * The same vocabulary an agent's document speaks: "provisioning" |
   * "running" | "idle" | "awaiting_input" | "failed" | "cancelled". A sub
   * session is talked to rather than delegated to, so it never reads
   * "completed".
   */
  status: string;
  createdAtMs: number;
  /**
   * When this sub session last did anything: the moment of its most recent
   * status change, which is the end of its last turn once it is idle again.
   *
   * A session has no *end* — nothing closes it — so this is not one.
   * It is how far along it got, which is what a reader looking at the
   * session's shape actually needs; without it a sub session can only be
   * drawn as running forever. Zero before it has moved at all.
   */
  lastActivityMs: number;
}