
import { StepRunView } from './stepRunView';
/**
 * One node of the run graph: a step of the definition, plus every execution
 */
export interface RunNode {
  step: string;
  runs: StepRunView[];
}