import type { ForkView } from "../api/types";

/** One fork, placed in the lineage the flat list describes. */
export type PlacedFork = {
  fork: ForkView;
  depth: number;
  /**
   * For each ancestor level, whether that ancestor still has a sibling below
   * this row — which is exactly what decides between drawing a rail in that
   * column and leaving it blank. Without it a deep fork's connection to its
   * grandparent either vanishes or runs through rows that are not its lineage.
   *
   * Always `depth` long.
   */
  rails: boolean[];
  /** Whether this is the last child of its parent — an elbow, not a tee. */
  last: boolean;
};

/**
 * Flatten a session's forks into render order: each fork immediately after the
 * one it branched from, carrying how deep it sits.
 *
 * The server sends them flat and parent-linked because that is how the registry
 * holds them — the nesting is the client's to derive, which is also what keeps
 * an arbitrarily deep chain from needing a recursive wire shape.
 *
 * Two cases that are not errors and must not disappear:
 *
 * - **A parent that resolves to nothing** renders at the top level. Deleting a
 *   fork does not delete its children, so a chain can outlive its root, and
 *   dropping those rows would hide conversations that are perfectly alive.
 * - **A cycle** cannot be produced by appending, but this walks data from a
 *   journal. Anything not reached by the descent is appended flat rather than
 *   silently omitted.
 */
export function forkTree(forks: ForkView[]): PlacedFork[] {
  const byParent = new Map<string, ForkView[]>();
  const ids = new Set(forks.map((f) => f.id));
  for (const f of forks) {
    // A parent nobody holds is the same as no parent, for placement.
    const key = f.parent && ids.has(f.parent) ? f.parent : "";
    const at = byParent.get(key);
    if (at) at.push(f);
    else byParent.set(key, [f]);
  }
  // Oldest first within a level, so a fork does not move because a sibling
  // renamed itself.
  for (const level of byParent.values()) {
    level.sort((a, b) => a.createdAtMs - b.createdAtMs || a.id.localeCompare(b.id));
  }

  const out: PlacedFork[] = [];
  const seen = new Set<string>();
  const walk = (parent: string, depth: number, rails: boolean[]) => {
    // Filtered up front rather than skipped in the loop: `last` is a property
    // of the rows actually drawn at this level, and a cycle's already-placed
    // node would otherwise make the final row claim it has a sibling below.
    const children = (byParent.get(parent) ?? []).filter((f) => !seen.has(f.id));
    children.forEach((fork, i) => {
      // The descent below can reach a node between the filter above and here
      // if the data is not a tree. Kept from the version before rails: this
      // walks a journal, and placing a row twice is worse than misplacing it.
      if (seen.has(fork.id)) return;
      seen.add(fork.id);
      const last = i === children.length - 1;
      out.push({ fork, depth, rails, last });
      walk(fork.id, depth + 1, [...rails, !last]);
    });
  };
  walk("", 0, []);

  // Whatever the descent could not reach — only possible if the data is not a
  // tree. Shown flat, because a conversation nobody can open is worse than one
  // shown in the wrong place.
  for (const fork of forks) {
    if (!seen.has(fork.id)) {
      out.push({ fork, depth: 0, rails: [], last: true });
    }
  }
  return out;
}
