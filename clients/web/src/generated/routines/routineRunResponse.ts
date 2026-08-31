
import { SessionSummary } from '../session';
/**
 * The session a trigger created — a plain session for an agent routine, a
 * workflow run for a workflow one. It is running in the background by the time
 * this is returned.
 */
export interface RoutineRunResponse {
  session: SessionSummary;
}