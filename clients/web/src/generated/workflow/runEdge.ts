
/**
 * One edge of the run graph: a transition of the definition, plus which
 */
export interface RunEdge {
  from: string;
  to: string;
  condition?: string;
  traversals: number[];
}