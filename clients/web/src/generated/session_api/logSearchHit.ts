
import { LogEntryKind } from '../agent';
/**
 * One entry matching a transcript search.
 *
 * A position and just enough text to judge it by — never the entry itself. A
 * search exists to say *where to read*, and one that answered with whole
 * entries would cost the same context as the paging it is there to avoid.
 */
export interface LogSearchHit {
  /**
   * Where this entry sits in the log. Feed it back as `before` or `after`
   * to read around it.
   */
  seq: number;
  atMs: number;
  kind: LogEntryKind;
  /**
   * The match with a little text either side, capped by the server.
   */
  snippet: string;
}