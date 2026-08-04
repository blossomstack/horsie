
import { SessionSummary } from '../session';
/**
 * A workflow&#x27;s runs, newest first.
 */
export interface WorkflowRunsResponse {
  sessions: SessionSummary[];
}