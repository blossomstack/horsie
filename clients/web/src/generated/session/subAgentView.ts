
/**
 * One agent this session hosts. The main agent has `parent`/`label` absent
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → the session&#x27;s main agent.
   */
  parent?: string;
  label?: string;
  depth: number;
  /**
   * &quot;running&quot; | &quot;completed&quot; | &quot;failed&quot;.
   */
  status: string;
  error?: string;
  /**
   * When this agent was spawned and when it reached its current result.
   */
  spawnedAtMs: number;
  endedAtMs: number;
}