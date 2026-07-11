
import { SessionStatusKind } from './sessionStatusKind';
/**
 * Live status transition. Sent without an SSE id (the session detail endpoint
 */
export interface StatusChangedEvent {
  status: SessionStatusKind;
  reason?: string;
}