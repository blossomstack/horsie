
import { SessionSummary } from '../session';
/**
 * The session a trigger created. It is running in the background by the time
 * this is returned.
 */
export interface RoutineRunResponse {
  session: SessionSummary;
}