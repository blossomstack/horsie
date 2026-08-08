
/**
 * Re-run one step execution. The new attempt appends to the run log; earlier
 * attempts are kept. A live run's current step is cancelled first.
 */
export interface WorkflowRetryRequest {
  stepIndex: number;
}