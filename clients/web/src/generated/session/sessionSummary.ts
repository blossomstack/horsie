
import { SessionStatusKind } from './sessionStatusKind';
export interface SessionSummary {
  id: string;
  name?: string;
  status: SessionStatusKind;
  createdAt: number;
  lastError?: string;
}