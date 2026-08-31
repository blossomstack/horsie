
import { AgentTarget } from './agentTarget';
import { WorkflowTarget } from './workflowTarget';
/**
 * What a routine runs when it fires.
 *
 * A union rather than two optional fields, so "neither" and "both" cannot be
 * written down. A routine that names nothing to run is not a routine, and one
 * that names two has no answer for which wins — neither is a state anything
 * downstream should have to handle.
 */
export type RoutineTarget =
  | { type: "Agent"; value: AgentTarget }
  | { type: "Workflow"; value: WorkflowTarget };