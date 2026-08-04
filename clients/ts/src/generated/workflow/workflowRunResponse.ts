
import { SessionSummary } from '../session';
/**
 * The session a run created. It is running by the time this is returned.
 */
export interface WorkflowRunResponse {
  session: SessionSummary;
}