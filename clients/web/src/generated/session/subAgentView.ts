
/**
 * One agent this session hosts. The main agent has `parent`/`label` absent
 * and `depth` 0; subagents carry their spawn metadata. `output` and the full
 * transcript live on the agent document and its history, not here.
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → the session's main agent.
   */
  parent?: string;
  label?: string;
  depth: number;
  /**
   * The plugin-declared agent type this subagent runs as, when it was
   * spawned with one. Absent for the main agent and for a general-purpose
   * subagent.
   */
  agentType?: string;
  /**
   * "running" | "completed" | "failed".
   */
  status: string;
  error?: string;
  /**
   * When this agent was spawned and when it reached its current result.
   * Zero when unrecorded — journaled before these were kept, or (for
   * `ended_at_ms`) still running.
   */
  spawnedAtMs: number;
  endedAtMs: number;
}