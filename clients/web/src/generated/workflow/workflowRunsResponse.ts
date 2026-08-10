
import { WorkflowRunSummary } from './workflowRunSummary';
/**
 * A workflow's runs, newest first.
 */
export interface WorkflowRunsResponse {
  runs: WorkflowRunSummary[];
}