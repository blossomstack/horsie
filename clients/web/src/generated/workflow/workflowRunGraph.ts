
import { RunEdge } from './runEdge';
import { RunNode } from './runNode';
/**
 * A run, projected onto the definition's graph.
 */
export interface WorkflowRunGraph {
  /**
   * The workflow this run was started from. The definition is snapshotted
   * at run creation, so editing or deleting it does not change this run.
   */
  workflow: string;
  /**
   * Index into the run log of the execution in flight.
   */
  current?: number;
  start: string;
  nodes: RunNode[];
  edges: RunEdge[];
  /**
   * The last step's output, once the run has finished.
   */
  output?: unknown;
  error?: string;
  /**
   * Every step's tokens, summed.
   */
  inputTokens: number;
  outputTokens: number;
}