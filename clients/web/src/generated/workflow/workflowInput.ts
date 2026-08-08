
import { WorkflowStepDef } from './workflowStepDef';
/**
 * Create or fully replace a workflow. `description` defaults to "".
 */
export interface WorkflowInput {
  name: string;
  description?: string;
  start: string;
  steps: WorkflowStepDef[];
}