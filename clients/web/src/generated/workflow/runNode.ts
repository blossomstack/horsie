
import { StepRunView } from './stepRunView';
/**
 * One node of the run graph: a step of the definition, plus every execution
 * that landed on it. `runs` is empty for a step the run never reached.
 */
export interface RunNode {
  step: string;
  runs: StepRunView[];
}