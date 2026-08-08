
import { AgentLogBody } from './agentLogBody';
/**
 * One item in an agent's log: the single ordered record a client reads.
 *
 * The agent actor is its only writer, which is what makes the order
 * deterministic without any merge — one actor, one mailbox, one fold, and
 * replay reproduces exactly what ran live.
 */
export interface AgentLogEntry {
  /**
   * Monotonic within this agent, assigned in the fold. This is the cursor.
   *
   * Stored rather than implied by position: the log is not front-trimmed
   * today, but storing it keeps trimming available for context management
   * and lets a cursor resolve by binary search instead of a scan.
   */
  seq: number;
  atMs: number;
  body: AgentLogBody;
}