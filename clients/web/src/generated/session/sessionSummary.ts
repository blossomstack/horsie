
import { SessionStatusKind } from './sessionStatusKind';
export interface SessionSummary {
  id: string;
  name?: string;
  /**
   * Absent when the session is not loaded: the server does not guess, and
   */
  status?: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * The workflow this session is a run of. Present only for a run, which is
   */
  workflow?: string;
}