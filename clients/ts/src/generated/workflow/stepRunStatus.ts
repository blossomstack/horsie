
import { StepCancelled } from './stepCancelled';
import { StepConcluded } from './stepConcluded';
import { StepFailed } from './stepFailed';
import { StepRunning } from './stepRunning';
/**
 * What became of one step execution.
 */
export type StepRunStatus =
  | { type: "Running"; value: StepRunning }
  | { type: "Concluded"; value: StepConcluded }
  | { type: "Failed"; value: StepFailed }
  | { type: "Cancelled"; value: StepCancelled };