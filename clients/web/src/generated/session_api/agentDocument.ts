
import { TaskItem } from '../agent';
import { Usage } from '../agent';
import { UsageView } from '../session';
/**
 * One agent&#39;s current values. The subagent-only fields (`parent`, `label`,
 */
export interface AgentDocument {
  id: string;
  /**
   * Parent agent id; absent → the session&#39;s main agent.
   */
  parent?: string;
  label?: string;
  /**
   * The task a subagent was spawned to do.
   */
  task?: string;
  depth: number;
  /**
   * &#34;running&#34; | &#34;completed&#34; | &#34;failed&#34;.
   */
  status: string;
  output?: string;
  error?: string;
  /**
   * The agent&#39;s `task_list` tool state.
   */
  tasks: TaskItem[];
  /**
   * Cumulative usage across this agent&#39;s completed turns.
   */
  usage: UsageView;
  /**
   * The most recently completed turn&#39;s own usage. Absent before the first.
   */
  lastTurnUsage?: Usage;
  /**
   * The last provider call&#39;s prompt size — what is loaded in context now.
   */
  contextTokens: number;
  /**
   * The model&#39;s configured context window, when known. Attached by the HTTP
   */
  contextWindow?: number;
  /**
   * The log position this document reflects.
   */
  asOfSeq: number;
}