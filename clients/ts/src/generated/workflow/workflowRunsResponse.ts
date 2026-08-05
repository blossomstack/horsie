
import { SessionSummary } from '../session';
/**
 * A workflow&#39;s runs, newest first.
 */
export interface WorkflowRunsResponse {
  sessions: SessionSummary[];
}