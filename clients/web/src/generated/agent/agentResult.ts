
import { CompletedOutput } from './completedOutput';
import { StoppedOutput } from './stoppedOutput';
/**
 * The outcome of an agent run
 */
export type AgentResult =
  | { type: "Completed"; value: CompletedOutput }
  | { type: "Stopped"; value: StoppedOutput };