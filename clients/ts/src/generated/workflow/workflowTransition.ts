
/**
 * A directed edge out of a step, optionally gated by a condition.
 *
 * `condition` is an expression evaluated against the producing step's
 * structured output, bound to `output`; it must evaluate to a boolean. A
 * `None` condition is an unconditional catch-all. Transitions are tried in
 * order and the first match wins. A step whose transitions all fail to match
 * ends the run, carrying that step's output as the run's.
 */
export interface WorkflowTransition {
  to: string;
  condition?: string;
}