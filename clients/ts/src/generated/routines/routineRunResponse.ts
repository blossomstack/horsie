
import { SessionSummary } from '../session';
/**
 * The session a trigger created. It is running in the background by the time
 */
export interface RoutineRunResponse {
  session: SessionSummary;
}