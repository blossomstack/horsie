
import { RunEdge } from './runEdge';
import { RunNode } from './runNode';
import { WorkflowStatus } from './workflowStatus';
/**
 * A run, projected onto the definition&#39;s graph.
 */
export interface WorkflowRunGraph {
  /**
   * The workflow this run was started from. The definition is snapshotted
   */
  workflow: string;
  status: WorkflowStatus;
  /**
   * Index into the run log of the execution in flight.
   */
  current?: number;
  start: string;
  nodes: RunNode[];
  edges: RunEdge[];
  /**
   * The last step&#39;s output, once the run has finished.
   */
  output?: unknown;
  error?: string;
  /**
   * Every step&#39;s tokens, summed.
   */
  inputTokens: number;
  outputTokens: number;
}