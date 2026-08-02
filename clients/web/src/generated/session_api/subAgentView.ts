
/**
 * One node of a session&#x27;s subagent tree. `output` is deliberately absent —
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → the session&#x27;s main agent.
   */
  parent?: string;
  label: string;
  depth: number;
  /**
   * &quot;running&quot; | &quot;completed&quot; | &quot;failed&quot;.
   */
  status: string;
  error?: string;
}