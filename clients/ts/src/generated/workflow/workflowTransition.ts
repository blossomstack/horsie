
/**
 * A directed edge out of a step, optionally gated by a condition.
 */
export interface WorkflowTransition {
  to: string;
  condition?: string;
}