
/**
 * One agent this session hosts. The main agent has `parent`/`label` absent
 * and `depth` 0; subagents carry their spawn metadata; a workflow step is
 * labelled with the step it ran. `output` and the full transcript live on the
 * agent document and its history, not here.
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → rooted on whatever this session's primary
   * agent is: its main agent, or the step that spawned it.
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
   * The saved agent preset this agent's settings came from; absent when they
   * were supplied inline. Every kind of agent answers it: a main agent names
   * the preset it was invoked from, a workflow step names its own, and a
   * subagent or sub session names whichever it inherited.
   *
   * Here so one read of a session says which of its agents are runs of which
   * preset, rather than needing a lookup per agent.
   */
  preset?: string;
  /**
   * What became of this agent: "provisioning" | "running" | "idle" |
   * "awaiting_input" | "completed" | "failed" | "cancelled". A main agent
   * reports its session's state and never *completes*; a subagent or a step
   * runs to one of the three endings.
   */
  status: string;
  error?: string;
  /**
   * When this agent was spawned and when it reached its current result.
   * Zero when unrecorded — journaled before these were kept, still running,
   * or a main agent, which nothing spawned and which is as old as the
   * session's own `created_at`.
   */
  spawnedAtMs: number;
  endedAtMs: number;
}