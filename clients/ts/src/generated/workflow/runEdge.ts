
/**
 * One edge of the run graph: a transition of the definition, plus which
 * executions took it. `traversals` is empty for an edge never taken.
 */
export interface RunEdge {
  from: string;
  to: string;
  condition?: string;
  traversals: number[];
}