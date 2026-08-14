
import { StepField } from './stepField';
import { StepOutcome } from './stepOutcome';
import { WorkflowTransition } from './workflowTransition';
/**
 * One step in a workflow graph.
 */
export interface WorkflowStepDef {
  name: string;
  /**
   * Agent preset this step runs as.
   */
  agent: string;
  /**
   * The step's instruction. Whatever the step is handed — the run's input
   * for the start step, the previous step's result for every other — is
   * appended below it under a header.
   */
  prompt: string;
  /**
   * The values this step's `outcome` may take. Absent → success / failure.
   *
   * A step finishes by calling `submit_result`, whose input schema is
   * compiled from these plus `fields` and a required markdown `description`.
   * Transitions read `outcome` and nothing else.
   */
  outcomes?: StepOutcome[];
  /**
   * Extra result fields, beyond `outcome` and `description`.
   */
  fields?: StepField[];
  /**
   * Whether this step may ask the person a question. Absent → false, and
   * the step has no `ask_user` tool at all.
   */
  interactive?: boolean;
  /**
   * Outgoing transitions, matched against this step's `outcome`.
   */
  transitions?: WorkflowTransition[];
  /**
   * Cap on agent-loop iterations for this step.
   */
  maxIterations?: number;
  /**
   * Retry budget for transient provider errors within this step.
   */
  maxRetries?: number;
}