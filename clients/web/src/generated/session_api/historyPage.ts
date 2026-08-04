
import { HistoryEntry } from '../agent';
/**
 * One window of an agent&#x27;s transcript, served from the agent&#x27;s in-memory
 */
export interface HistoryPage {
  entries: HistoryEntry[];
  hasMoreBefore: boolean;
  hasMoreAfter: boolean;
}