
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
   * for the start step, the previous step's output for every other — is
   * appended below it under a header.
   */
  prompt: string;
  /**
   * JSON Schema for the step's structured output. When present, the step
   * finishes by calling the builtin terminal tool with output conforming to
   * it. Required when the step has any conditional transition, since there
   * would otherwise be nothing for the condition to read.
   */
  outputSchema?: unknown;
  /**
   * Outgoing transitions, evaluated against this step's output.
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