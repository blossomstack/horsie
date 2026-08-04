
/**
 * A finished subagent&#x27;s result, delivered to the agent that spawned it.
 */
export interface SubAgentResultPart {
  subagentId: string;
  label: string;
  /**
   * &quot;completed&quot; | &quot;failed&quot; — the SubAgentView.status vocabulary.
   */
  status: string;
  /**
   * Output on success, error text on failure. Already capped at 50 KB by
   */
  text: string;
  /**
   * When the subagent was spawned and when it reached this result. Zero on
   */
  spawnedAtMs: number;
  endedAtMs: number;
}