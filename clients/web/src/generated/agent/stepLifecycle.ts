
/**
 * One workflow step&#39;s progress, recorded on that step&#39;s own agent. Carries the
 */
export interface StepLifecycle {
  index: number;
  name: string;
  status: string;
}