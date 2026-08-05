
import { SessionStartOutcome } from './sessionStartOutcome';
/**
 * `source` is the matcher domain, and the record keeps the wire spelling the
 */
export interface SessionStartRecord {
  source: string;
  systemMessage?: string;
  outcome: SessionStartOutcome;
}