
import { HistoryEntry } from '../agent';
/**
 * One window of an agent&#39;s transcript, served from the agent&#39;s in-memory
 */
export interface HistoryPage {
  entries: HistoryEntry[];
  hasMoreBefore: boolean;
  hasMoreAfter: boolean;
}