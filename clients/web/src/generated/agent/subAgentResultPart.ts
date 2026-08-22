
/**
 * A finished subagent's result, delivered to the agent that spawned it.
 * Carried as its own part rather than merged into the user text so a client
 * can render it as agent activity instead of as something the person said;
 * providers flatten it back to the same text block they have always received.
 */
export interface SubAgentResultPart {
  subagentId: string;
  /**
   * What the subagent was called: the `title` its spawner gave it.
   */
  title: string;
  /**
   * "completed" | "failed" — the SubAgentView.status vocabulary.
   */
  status: string;
  /**
   * Output on success, error text on failure. Already capped at 50 KB by
   * the session, truncation marker included.
   */
  text: string;
  /**
   * When the subagent was spawned and when it reached this result. Zero on
   * rows journaled before these were recorded — a client shows no duration
   * rather than an invented one.
   */
  spawnedAtMs: number;
  endedAtMs: number;
}