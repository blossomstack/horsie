
/**
 * A finished subagent&#39;s result, delivered to the agent that spawned it.
 */
export interface SubAgentResultPart {
  subagentId: string;
  label: string;
  /**
   * &#34;completed&#34; | &#34;failed&#34; — the SubAgentView.status vocabulary.
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