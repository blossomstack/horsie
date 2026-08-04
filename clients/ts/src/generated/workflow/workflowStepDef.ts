
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
   * The step&#x27;s instruction. Whatever the step is handed — the run&#x27;s input
   */
  prompt: string;
  /**
   * JSON Schema for the step&#x27;s structured output. When present, the step
   */
  outputSchema?: unknown;
  /**
   * Outgoing transitions, evaluated against this step&#x27;s output.
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