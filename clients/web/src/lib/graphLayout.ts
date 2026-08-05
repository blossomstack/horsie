/**
 * Laying out a workflow graph.
 *
 * Rank assignment by breadth-first walk from the start step: a node's rank is
 * one past the shallowest step that reaches it. Edges that point back to an
 * equal or lower rank are back-edges — loops — and are drawn as curves so they
 * read as returns rather than as forward progress.
 *
 * Hand-rolled rather than dagre: a step-list editor produces well under twenty
 * nodes, and this is the whole algorithm. If a real definition ever lays out
 * badly, swapping in dagre means replacing this one pure function.
 */

/** The shape the layout needs — a subset of both the editor draft and the run graph. */
export interface LayoutNode {
  step: string;
}

export interface LayoutEdge {
  from: string;
  to: string;
}

export interface PlacedNode {
  step: string;
  /** Distance from the start step, in edges. */
  rank: number;
  /** Position within the rank, left to right. */
  order: number;
  /** Whether the start step reaches this node at all. */
  reachable: boolean;
}

export interface PlacedEdge {
  from: string;
  to: string;
  /** Points back to an equal or shallower rank: a loop. */
  back: boolean;
}

export interface Layout {
  nodes: PlacedNode[];
  edges: PlacedEdge[];
  /** Nodes in the fullest rank — the graph's cross-axis size. */
  breadth: number;
  /** Number of ranks — the graph's flow-axis size. */
  depth: number;
}

/**
 * Place every node and classify every edge.
 *
 * Unreachable nodes are kept, not dropped: a step the start cannot reach is
 * usually a mistake the author wants to see, and silently hiding it would make
 * the graph disagree with the step list beside it. They are ranked last.
 */
export function layoutGraph(
  nodes: LayoutNode[],
  edges: LayoutEdge[],
  start: string,
): Layout {
  const names = nodes.map((n) => n.step);
  const known = new Set(names);
  const outgoing = new Map<string, string[]>();
  for (const e of edges) {
    if (!known.has(e.from) || !known.has(e.to)) continue;
    const list = outgoing.get(e.from);
    if (list) list.push(e.to);
    else outgoing.set(e.from, [e.to]);
  }

  const rank = new Map<string, number>();
  if (known.has(start)) {
    rank.set(start, 0);
    let frontier = [start];
    while (frontier.length > 0) {
      const next: string[] = [];
      for (const name of frontier) {
        const depth = rank.get(name) ?? 0;
        for (const target of outgoing.get(name) ?? []) {
          if (rank.has(target)) continue;
          rank.set(target, depth + 1);
          next.push(target);
        }
      }
      frontier = next;
    }
  }

  // Unreachable steps sit one rank past everything the start reaches, in the
  // order the definition lists them.
  const deepest = rank.size > 0 ? Math.max(...rank.values()) : -1;
  const unreachableRank = deepest + 1;

  const placed: PlacedNode[] = [];
  const counts = new Map<number, number>();
  for (const name of names) {
    const reachable = rank.has(name);
    const r = reachable ? (rank.get(name) as number) : unreachableRank;
    const order = counts.get(r) ?? 0;
    counts.set(r, order + 1);
    placed.push({ step: name, rank: r, order, reachable });
  }

  const rankOf = new Map(placed.map((n) => [n.step, n.rank]));
  const placedEdges: PlacedEdge[] = edges
    .filter((e) => known.has(e.from) && known.has(e.to))
    .map((e) => {
      const from = rankOf.get(e.from) ?? 0;
      const to = rankOf.get(e.to) ?? 0;
      return { from: e.from, to: e.to, back: to <= from };
    });

  return {
    nodes: placed,
    edges: placedEdges,
    breadth: counts.size === 0 ? 0 : Math.max(...counts.values()),
    depth: counts.size,
  };
}
