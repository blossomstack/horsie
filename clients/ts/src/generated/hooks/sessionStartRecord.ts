
import { SessionStartOutcome } from './sessionStartOutcome';
/**
 * `source` is the matcher domain, and the record keeps the wire spelling the
 * hook was given rather than the enum: this is what the transcript shows.
 */
export interface SessionStartRecord {
  source: string;
  systemMessage?: string;
  outcome: SessionStartOutcome;
}