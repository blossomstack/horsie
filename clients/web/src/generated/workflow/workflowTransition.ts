
import { OutcomeFilter } from './outcomeFilter';
/**
 * A directed edge out of a step, optionally gated by the outcome it produced.
 *
 * `when` is matched against the producing step's `outcome`. Absent is an
 * unconditional catch-all. Transitions are tried in order and the first match
 * wins. A step whose transitions all fail to match ends the run, carrying that
 * step's result as the run's.
 */
export interface WorkflowTransition {
  to: string;
  when?: OutcomeFilter;
}