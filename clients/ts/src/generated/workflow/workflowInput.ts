
import { WorkflowStepDef } from './workflowStepDef';
/**
 * Create or fully replace a workflow. `description` defaults to &#34;&#34;.
 */
export interface WorkflowInput {
  name: string;
  description?: string;
  start: string;
  steps: WorkflowStepDef[];
}