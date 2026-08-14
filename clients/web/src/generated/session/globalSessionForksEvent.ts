
import { ForkView } from './forkView';
/**
 * A forks frame on the global `/api/events` stream.
 *
 * Carries the session's whole roster rather than a delta, because that is what
 * the registry holds and what `List` would answer: a client replaces the rows
 * it has instead of reconciling an add against a status change it may have
 * missed. The same reason `SessionRecord.forks` is a `Vec` and not an event log.
 */
export interface GlobalSessionForksEvent {
  sessionId: string;
  forks: ForkView[];
}