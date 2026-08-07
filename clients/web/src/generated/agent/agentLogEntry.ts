
import { AgentLogBody } from './agentLogBody';
/**
 * One item in an agent&#39;s log: the single ordered record a client reads.
 */
export interface AgentLogEntry {
  /**
   * Monotonic within this agent, assigned in the fold. This is the cursor.
   */
  seq: number;
  atMs: number;
  body: AgentLogBody;
}