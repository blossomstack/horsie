
import { SessionStatusKind } from './sessionStatusKind';
/**
 * One frame on the global `/api/events` stream (live session-list updates).
 */
export interface GlobalSessionEvent {
  sessionId: string;
  status: SessionStatusKind;
  reason?: string;
}