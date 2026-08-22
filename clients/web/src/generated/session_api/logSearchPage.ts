
import { LogSearchHit } from './logSearchHit';
/**
 * Matches for one transcript search, oldest first.
 *
 * Ordered oldest-first even though pages read newest-first: matches are read
 * as a set to choose from rather than scrolled, and a run's story goes
 * forwards.
 */
export interface LogSearchPage {
  hits: LogSearchHit[];
}