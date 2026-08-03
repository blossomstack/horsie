
import { Usage } from '../agent';
import { TaskItem } from '../session';
import { UsageView } from '../session';
/**
 * One agent&#x27;s current values. The subagent-only fields (`parent`, `label`,
 */
export interface AgentDocument {
  id: string;
  /**
   * Parent agent id; absent → the session&#x27;s main agent.
   */
  parent?: string;
  label?: string;
  /**
   * The task a subagent was spawned to do.
   */
  task?: string;
  depth: number;
  /**
   * &quot;running&quot; | &quot;completed&quot; | &quot;failed&quot;.
   */
  status: string;
  output?: string;
  error?: string;
  /**
   * The agent&#x27;s `task_list` tool state.
   */
  tasks: TaskItem[];
  /**
   * Cumulative usage across this agent&#x27;s completed turns.
   */
  usage: UsageView;
  /**
   * The most recently completed turn&#x27;s own usage. Absent before the first.
   */
  lastTurnUsage?: Usage;
  /**
   * The last provider call&#x27;s prompt size — what is loaded in context now.
   */
  contextTokens: number;
  /**
   * The model&#x27;s configured context window, when known. Attached by the HTTP
   */
  contextWindow?: number;
}