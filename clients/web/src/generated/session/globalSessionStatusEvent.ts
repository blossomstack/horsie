
import { SessionStatusKind } from './sessionStatusKind';
/**
 * A status frame on the global `/api/events` stream.
 */
export interface GlobalSessionStatusEvent {
  sessionId: string;
  status: SessionStatusKind;
  reason?: string;
}