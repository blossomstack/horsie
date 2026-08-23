
import { AgentStats } from './agentStats';
/**
 * One agent this session hosts — enough of it to draw the session's shape and
 * to answer for any one agent without reading its transcript.
 *
 * Everything here is folded from the session's own journal, so a roster of
 * thirty agents costs no agent recoveries. The transcript is the one thing
 * that is not: it is read per agent, at `/sessions/:id/agents/:id/messages`.
 */
export interface SubAgentView {
  id: string;
  /**
   * Parent agent id; absent → rooted on whatever this session's primary
   * agent is: its main agent, or the step that spawned it.
   */
  parent?: string;
  /**
   * What this agent is called. For the main agent it is the session's name,
   * because the main agent *is* the session; for a subagent it is the title
   * its spawner gave it; for a workflow step it is the step's name. Absent
   * only while nothing has named it — a session whose first turn has not
   * produced a title yet.
   */
  title?: string;
  /**
   * What kind of agent this is: "main" | "subagent" | "step". A sub session
   * is not here — it has its own list, for the reason `subSessions` gives.
   */
  kind: string;
  /**
   * What it was asked to do: a subagent's task. Absent for the main agent,
   * which is talked to turn by turn rather than briefed once, and for a
   * step, whose brief is its definition's.
   */
  input?: string;
  /**
   * What it produced, once it has. Only delegated work has one: a
   * subagent's report or a step's output. Never the main agent's — a
   * session concludes nothing.
   */
  output?: string;
  /**
   * Its banked numbers.
   */
  stats: AgentStats;
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
  /**
   * The workflow run this execution belongs to, for a step; absent for
   * every other kind.
   *
   * A session can host more than one: its own, if it is a run, and one per
   * `invoke_workflow` any of its agents called. Without this they arrive as
   * one flat list of steps, and a client redrawing the run as the sequence
   * it is has no way to tell two runs apart — or to tell an invoked run
   * from the session's own.
   */
  run?: string;
  /**
   * The workflow that run was started from — its name, for display. On
   * every step of the run rather than in a list of its own, so one read of
   * a session is still all a picture of it needs.
   */
  workflow?: string;
}