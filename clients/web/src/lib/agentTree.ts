import { MAIN_AGENT } from "../api/client";
import type { SubAgentView } from "../api/types";

/**
 * Placing a session's agents as a tree.
 *
 * The roster arrives flat and parent-linked — the same shape `forkTree` reads,
 * for the same reason: the nesting is the client's to derive, which keeps an
 * arbitrarily deep chain off the wire. What is different here is that the
 * picture has drawn edges, so this produces coordinates rather than indents:
 * depth on one axis, a tidy walk on the other, exactly like `graphLayout` does
 * for a workflow.
 *
 * Positions come out in rows and ranks rather than pixels. What a row is worth
 * is the renderer's business, and a layout that has already multiplied by it
 * cannot be tested without knowing the constants.
 */

/** One agent, placed in the tree its session's roster describes. */
export interface PlacedAgent {
  id: string;
  /** Its own label, else the preset it runs, else what it is. */
  label: string;
  /** The wire status — `running`, `completed`, `failed`, … */
  status: string;
  /** The preset it runs, when it has one. */
  agentType: string | null;
  /** Status, duration and start: what the node itself has no room for. */
  detail: string;
  /** The session's own agent, or something the session spawned. */
  kind: "main" | "subagent";
  /** Distance from the main agent, in edges. */
  depth: number;
  /**
   * Cross-axis position, in rows. A leaf takes the next free row; a parent
   * sits at the midpoint of its children, so it is fractional whenever it has
   * an even number of them.
   */
  lane: number;
  /** The agent this hangs off — null only for the main agent. */
  parent: string | null;
  /** Children in the roster: what the fold toggle would disclose. */
  children: number;
  /** Every agent below this one, folded or not — what a fold stands for. */
  descendants: number;
  /** Folded: its children are in the roster but not in `nodes`. */
  collapsed: boolean;
}

export interface AgentEdge {
  from: string;
  to: string;
}

export interface AgentTree {
  /** Drawn agents, in tree order. A folded agent's children are not here. */
  nodes: PlacedAgent[];
  /** One per drawn parent-child pair. */
  edges: AgentEdge[];
  /** Ranks the tree occupies — deepest depth plus one. */
  depth: number;
  /** Rows the tree occupies. */
  rows: number;
  /** Agents in the roster that the fold is hiding. */
  hidden: number;
}

/** Statuses that mean the agent has not stopped, so it has no duration yet. */
const LIVE_STATUS = new Set(["running", "provisioning", "awaiting_input"]);

/**
 * Lay a session's agents out as a tree, minus whatever is folded away.
 *
 * `collapsed` names agents whose children are not to be drawn. It is passed in
 * rather than held here because it is view state — the page owns it, and the
 * timeline beside this reads the same list, so folding an agent in one view
 * folds it in the other.
 *
 * The two cases `forkTree` learned are the same here, because this reads the
 * same journal-derived data: an agent whose parent nobody holds roots at the
 * top level rather than vanishing, and anything a descent cannot reach is
 * appended flat rather than silently dropped.
 */
export function layoutAgentTree(
  agents: SubAgentView[],
  collapsed: readonly string[] = [],
): AgentTree {
  if (agents.length === 0) {
    return { nodes: [], edges: [], depth: 0, rows: 0, hidden: 0 };
  }

  // The main agent is the one nothing spawned. The same fallbacks the timeline
  // uses, so the two views agree on which agent is the root.
  const main = agents.find((a) => !a.parent && a.depth === 0) ?? agents[0];
  const mainId = main?.id ?? MAIN_AGENT;

  const held = new Set(agents.map((a) => a.id));
  /** Children by parent id; `""` is the main agent's own bucket.
   *
   * A top-level subagent reaches us with no parent at all — the schema says an
   * absent parent means "rooted on the session's primary agent" — but one that
   * names the main agent outright means the same thing, and both have to land
   * in the same bucket or one of the two conventions draws a forest. */
  const kids = new Map<string, SubAgentView[]>();
  for (const a of agents) {
    if (a.id === mainId) continue;
    const linked = a.parent && a.parent !== a.id && held.has(a.parent);
    const key = linked && a.parent !== mainId ? (a.parent ?? "") : "";
    kids.set(key, [...(kids.get(key) ?? []), a]);
  }
  // Oldest first, so an agent does not move because a sibling was relabelled.
  for (const level of kids.values()) {
    level.sort((x, y) => x.spawnedAtMs - y.spawnedAtMs || x.id.localeCompare(y.id));
  }
  const bucket = (id: string) => (id === mainId ? "" : id);

  const descendantsOf = countDescendants(kids, bucket);

  // Rows are handed out as the walk reaches leaves, so the tree reads top to
  // bottom in the order it was spawned.
  let nextLane = 0;
  const reached = new Set<string>([mainId]);

  interface Subtree {
    node: PlacedAgent;
    below: Subtree[];
  }

  /** Everything under `id`, marked as accounted for without being drawn.
   *
   * A fold has to swallow the whole subtree, not just the row below it: the
   * pass that rescues unreachable agents cannot tell "hidden on purpose" from
   * "lost", so a grandchild left unmarked comes back as an orphan hanging off
   * the main agent — the one thing folding is supposed to prevent. */
  const swallow = (id: string) => {
    for (const child of kids.get(bucket(id)) ?? []) {
      if (reached.has(child.id)) continue;
      reached.add(child.id);
      swallow(child.id);
    }
  };

  const place = (
    agent: SubAgentView | null,
    id: string,
    depth: number,
    parent: string | null,
  ): Subtree => {
    // Filtered against `reached` rather than trusted: this walks a journal, and
    // a cycle must not put an agent in two places.
    const children = (kids.get(bucket(id)) ?? []).filter((c) => !reached.has(c.id));
    for (const c of children) reached.add(c.id);

    const folded = children.length > 0 && collapsed.includes(id);
    if (folded) for (const c of children) swallow(c.id);
    const below = folded
      ? []
      : children.map((c) => place(c, c.id, depth + 1, id));

    // A leaf — or a folded agent, which is a leaf as far as the picture goes —
    // takes the next row. Everything else centres on the children it spans.
    const first = below[0]?.node.lane;
    const last = below[below.length - 1]?.node.lane;
    const lane =
      first === undefined || last === undefined ? nextLane++ : (first + last) / 2;

    const isMain = id === mainId;
    return {
      node: {
        id,
        // The main agent is named for what it is. It is the session, and a
        // session already has a title at the top of the page.
        label: isMain ? "main agent" : (agent?.label ?? agent?.agentType ?? "subagent"),
        status: agent?.status ?? "idle",
        agentType: agent?.agentType ?? null,
        detail: agent
          ? describeAgent(agent.status, agent.spawnedAtMs, agent.endedAtMs)
          : "idle",
        kind: isMain ? "main" : "subagent",
        depth,
        lane,
        parent,
        children: children.length,
        descendants: descendantsOf(id),
        collapsed: folded,
      },
      below,
    };
  };

  const root = place(main ?? null, mainId, 0, null);

  const nodes: PlacedAgent[] = [];
  const edges: AgentEdge[] = [];
  const flatten = (t: Subtree) => {
    nodes.push(t.node);
    for (const child of t.below) {
      edges.push({ from: t.node.id, to: child.node.id });
      flatten(child);
    }
  };
  flatten(root);

  // Only reachable if the roster is not a tree. Shown hanging off the main
  // agent, because an agent nobody can find is worse than one drawn in the
  // wrong place.
  for (const a of agents) {
    if (reached.has(a.id)) continue;
    reached.add(a.id);
    nodes.push({
      id: a.id,
      label: a.label ?? a.agentType ?? "subagent",
      status: a.status,
      agentType: a.agentType ?? null,
      detail: describeAgent(a.status, a.spawnedAtMs, a.endedAtMs),
      kind: "subagent",
      depth: 1,
      lane: nextLane++,
      parent: mainId,
      children: 0,
      descendants: 0,
      collapsed: false,
    });
    edges.push({ from: mainId, to: a.id });
  }

  const drawn = new Set(nodes.map((n) => n.id));
  return {
    nodes,
    edges,
    depth: nodes.reduce((d, n) => Math.max(d, n.depth + 1), 0),
    rows: Math.max(nextLane, 1),
    hidden: agents.filter((a) => !drawn.has(a.id)).length,
  };
}

/**
 * How many agents sit below each one, memoised.
 *
 * Counted over the whole roster rather than over what is drawn: the number is
 * what a folded node reports, so folding must not change it.
 */
function countDescendants(
  kids: Map<string, SubAgentView[]>,
  bucket: (id: string) => string,
): (id: string) => number {
  const memo = new Map<string, number>();
  const walk = (id: string, above: Set<string>): number => {
    const done = memo.get(id);
    if (done !== undefined) return done;
    // A cycle counts as nothing rather than recurring forever.
    if (above.has(id)) return 0;
    const next = new Set(above).add(id);
    const total = (kids.get(bucket(id)) ?? []).reduce(
      (n, child) => n + 1 + walk(child.id, next),
      0,
    );
    memo.set(id, total);
    return total;
  };
  return (id: string) => walk(id, new Set());
}

/** What became of an agent, how long it took, and when it started. */
function describeAgent(status: string, startMs: number, endMs: number): string {
  const parts = [status.replace(/_/g, " ")];
  if (startMs > 0 && endMs > startMs && !LIVE_STATUS.has(status)) {
    parts.push(humanMs(endMs - startMs));
  }
  if (startMs > 0) parts.push(`started ${clockTime(startMs)}`);
  return parts.join(" · ");
}

function humanMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${Math.floor(ms / 3_600_000)}h ${Math.round((ms % 3_600_000) / 60_000)}m`;
}

/** 24-hour, so a label is five characters wide however long the session ran. */
function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}
