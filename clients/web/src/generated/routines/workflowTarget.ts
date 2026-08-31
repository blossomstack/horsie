
/**
 * Start a run of a workflow, with the routine's prompt as its input. The run's
 * steps supply their own presets, exactly as an interactive run's do.
 */
export interface WorkflowTarget {
  workflow: string;
}