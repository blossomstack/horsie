
import { TaskItem } from '../agent';
import { Usage } from '../agent';
import { UsageView } from '../session';
/**
 * One agent's current values: what it is, what became of it, what it runs
 * under, and its live numbers. The subagent-only fields (`parent`, `title`,
 * `task`, `output`, `error`) are absent for a session's main agent.
 *
 * The configuration fields — `model`, `mcp_servers`, `memory_spaces`,
 * `use_plugins`, `thinking_effort` — are this agent's own, resolved from
 * what it runs under: a workflow step's are its preset's, never the
 * session's. The session document deliberately carries no session-wide model.
 */
export interface AgentDocument {
  id: string;
  /**
   * Parent agent id; absent → rooted on whatever this session's primary
   * agent is: its main agent, or the step that spawned it.
   */
  parent?: string;
  /**
   * What this agent is called: the main agent's title (which is the
   * session's name), a subagent's or sub session's, or the step a workflow
   * agent ran.
   */
  title?: string;
  /**
   * The task a subagent was spawned to do.
   */
  task?: string;
  depth: number;
  /**
   * What became of this agent. The same vocabulary the session's roster
   * speaks: "provisioning" | "running" | "idle" | "awaiting_input" |
   * "completed" | "failed" | "cancelled".
   */
  status: string;
  output?: string;
  error?: string;
  /**
   * The model this agent runs under: the main agent's, a step's own
   * preset, or a subagent's inherited tree root.
   */
  model: string;
  /**
   * Names of the MCP servers this agent may call (empty when none).
   */
  mcpServers: string[];
  /**
   * Memory spaces this agent may read and write (empty when none).
   */
  memorySpaces: string[];
  /**
   * The built-in tools this agent may call, frozen at creation. Absent → the
   * server's default set, which is what an unnarrowed session has.
   *
   * Present so a session that refuses a tool call can say what it was
   * launched with. Without it the only symptom of a narrowed selection is a
   * tool that will not run and nothing to check it against.
   */
  allowedTools?: string[];
  /**
   * Whether the runtime's plugin/skill machinery is enabled for this agent.
   */
  usePlugins: boolean;
  /**
   * This agent's thinking effort, frozen at creation or inherited from the
   * model's default. Absent → the model exposes no thinking control.
   */
  thinkingEffort?: string;
  /**
   * The agent's `task_list` tool state.
   */
  tasks: TaskItem[];
  /**
   * Cumulative usage across this agent's completed turns.
   */
  usage: UsageView;
  /**
   * The most recently completed turn's own usage. Absent before the first.
   */
  lastTurnUsage?: Usage;
  /**
   * The last provider call's prompt size — what is loaded in context now.
   * Never summed across turns.
   */
  contextTokens: number;
  /**
   * The model's configured context window, when known. Attached by the HTTP
   * layer from model config — this one field is not agent state.
   */
  contextWindow?: number;
  /**
   * The log position this document reflects.
   *
   * What makes reading this alongside the log safe rather than a race: a
   * consumer records the seq each value was last set from and applies an
   * update only if its seq is greater — whichever source it came from. A
   * boolean "a live frame has arrived" latch cannot express that, because it
   * cannot tell a document that is *ahead* of the fold from one behind it.
   */
  asOfSeq: number;
}