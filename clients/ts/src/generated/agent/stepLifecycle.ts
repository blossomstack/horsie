
/**
 * One workflow step's progress, recorded on that step's own agent. Carries the
 * step's `name` as well as its index: the index identifies the execution, but
 * the name is what a person reading the run recognises.
 */
export interface StepLifecycle {
  index: number;
  name: string;
  status: string;
}