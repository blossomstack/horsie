
/**
 * One edge of the run graph: a transition of the definition, plus which
 * executions took it. `traversals` is empty for an edge never taken.
 */
export interface RunEdge {
  from: string;
  to: string;
  /**
   * The filter this edge is taken for, rendered for display — `outcome in
   * [p0, p1]`. Absent on a catch-all.
   */
  condition?: string;
  traversals: number[];
}