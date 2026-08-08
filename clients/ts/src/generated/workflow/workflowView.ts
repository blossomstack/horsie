
import { WorkflowStepDef } from './workflowStepDef';
/**
 * A workflow as shown to clients.
 */
export interface WorkflowView {
  /**
   * Slug; the id of record, used in API paths.
   */
  name: string;
  description: string;
  /**
   * Name of the step every run begins at.
   */
  start: string;
  steps: WorkflowStepDef[];
  /**
   * Most step executions one run may perform before it is failed. Absent →
   * the server's default.
   */
  maxSteps?: number;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}