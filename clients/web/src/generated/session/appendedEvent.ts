
import { HistoryEntry } from '../agent';
/**
 * One transcript append — a user message, an assistant message, or a tool
 */
export interface AppendedEvent {
  entry: HistoryEntry;
}