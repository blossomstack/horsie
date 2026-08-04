
/**
 * Re-run one step execution. The new attempt appends to the run log; earlier
 */
export interface WorkflowRetryRequest {
  stepIndex: number;
}