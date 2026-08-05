
/**
 * One agent this session hosts. The main agent has `parent`/`label` absent
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → the session&#39;s main agent.
   */
  parent?: string;
  label?: string;
  depth: number;
  /**
   * &#34;running&#34; | &#34;completed&#34; | &#34;failed&#34;.
   */
  status: string;
  error?: string;
  /**
   * When this agent was spawned and when it reached its current result.
   */
  spawnedAtMs: number;
  endedAtMs: number;
}