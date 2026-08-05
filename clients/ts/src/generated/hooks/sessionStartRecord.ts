
import { SessionStartOutcome } from './sessionStartOutcome';
/**
 * `source` is the matcher domain: startup | resume | clear | compact | fork.
 */
export interface SessionStartRecord {
  source: string;
  systemMessage?: string;
  outcome: SessionStartOutcome;
}