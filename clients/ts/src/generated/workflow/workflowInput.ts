
import { WorkflowStepDef } from './workflowStepDef';
/**
 * Create or fully replace a workflow. `description` defaults to "".
 */
export interface WorkflowInput {
  name: string;
  description?: string;
  start: string;
  steps: WorkflowStepDef[];
  /**
   * Most step executions one run may perform before it is failed; at least 1.
   * Absent → the server's default.
   *
   * The only thing bounding a graph whose loop condition never flips, which
   * is why it lives on the definition rather than on the run request: the
   * budget is a property of the graph's shape, and a workflow that
   * legitimately loops twenty times knows that about itself.
   */
  maxSteps?: number;
}