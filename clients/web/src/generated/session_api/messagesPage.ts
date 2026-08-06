
import { AgentLogEntry } from '../agent';
/**
 * One page of an agent&#39;s log — the `before`/`max` form of
 */
export interface MessagesPage {
  entries: AgentLogEntry[];
}