
import { SessionSummary } from '../session';
/**
 * A workflow's runs, newest first.
 */
export interface WorkflowRunsResponse {
  sessions: SessionSummary[];
}