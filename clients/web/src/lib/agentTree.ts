import { MAIN_AGENT } from "../api/client";
import type { SubAgentView, SubSessionView } from "../api/types";

/**
 * Placing everything a session hosts as one tree: its agents *and* its sub
 * sessions.
 *
 * Both rosters arrive flat and parent-linked — the nesting is the client's to
 * derive, which keeps an arbitrarily deep chain off the wire — and both name
 * ids out of the same space, so they lay out as one lineage rather than two
 * pictures. They have to: a subagent spawned by a sub session names that sub
 * session as its parent, and with only the agents in hand there was nothing to
 * hang it on, so it came out rooted on the main agent.
 *
 * What is different from `subSessionTree` is that the picture has drawn edges,
 * so this produces coordinates rather than indents: depth on one axis, a tidy
 * walk on the other, exactly like `graphLayout` does for a workflow.
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
  /**
   * The session's own agent, something it delegated to, or a session branched
   * from it. A sub session is not an agent the session spawned — it is talked
   * to, it owes nobody a result, and it is opened rather than inspected — so
   * the picture has to be able to say which it is drawing.
   */
  kind: "main" | "subagent" | "sub_session";
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
  /** Members of either roster that the fold is hiding. */
  hidden: number;
}

/** Statuses that mean the agent has not stopped, so it has no duration yet. */
const LIVE_STATUS = new Set(["running", "provisioning", "awaiting_input"]);

/** One drawable thing, from either roster, in the one shape the walk reads. */
interface Member {
  id: string;
  parent: string | undefined;
  label: string;
  status: string;
  agentType: string | null;
  detail: string;
  kind: "subagent" | "sub_session";
  /** When it came into being — the order siblings are drawn in. */
  at: number;
}

function agentMember(a: SubAgentView): Member {
  return {
    id: a.id,
    parent: a.parent,
    label: a.label ?? a.agentType ?? "subagent",
    status: a.status,
    agentType: a.agentType ?? null,
    detail: describeAgent(a.status, a.spawnedAtMs, a.endedAtMs),
    kind: "subagent",
    at: a.spawnedAtMs,
  };
}

/**
 * A sub session, as the same shape.
 *
 * Measured from when it was branched to when it last did anything, which is
 * what the timeline draws too. It has no *end* — nothing closes a session —
 * but "still running, forever" was a worse lie than "this is how far it got".
 */
function subSessionMember(s: SubSessionView): Member {
  return {
    id: s.id,
    parent: s.parent,
    label: s.title ?? "untitled sub session",
    status: s.status,
    agentType: null,
    detail: describeAgent(s.status, s.createdAtMs, s.lastActivityMs),
    kind: "sub_session",
    at: s.createdAtMs,
  };
}

/**
 * Lay everything a session hosts out as a tree, minus whatever is folded away.
 *
 * `collapsed` names members whose children are not to be drawn. It is passed in
 * rather than held here because it is view state — the page owns it, and the
 * timeline beside this reads the same list, so folding an agent in one view
 * folds it in the other.
 *
 * The two cases `subSessionTree` learned are the same here, because this reads
 * the same journal-derived data: a member whose parent nobody holds roots at
 * the top level rather than vanishing, and anything a descent cannot reach is
 * appended flat rather than silently dropped.
 */
export function layoutAgentTree(
  agents: SubAgentView[],
  subSessions: SubSessionView[] = [],
  collapsed: readonly string[] = [],
): AgentTree {
  if (agents.length === 0 && subSessions.length === 0) {
    return { nodes: [], edges: [], depth: 0, rows: 0, hidden: 0 };
  }

  // The main agent is the one nothing spawned. The same fallbacks the timeline
  // uses, so the two views agree on which agent is the root.
  const main = agents.find((a) => !a.parent && a.depth === 0) ?? agents[0];
  const mainId = main?.id ?? MAIN_AGENT;

  const members: Member[] = [
    ...agents.filter((a) => a.id !== mainId).map(agentMember),
    ...subSessions.map(subSessionMember),
  ];

  const held = new Set(members.map((m) => m.id));
  /** Children by parent id; `""` is the main agent's own bucket.
   *
   * A top-level subagent reaches us with no parent at all — the schema says an
   * absent parent means "rooted on the session's primary agent" — but one that
   * names the main agent outright means the same thing, and both have to land
   * in the same bucket or one of the two conventions draws a forest. */
  const kids = new Map<string, Member[]>();
  for (const m of members) {
    const linked = m.parent && m.parent !== m.id && held.has(m.parent);
    const key = linked && m.parent !== mainId ? (m.parent ?? "") : "";
    kids.set(key, [...(kids.get(key) ?? []), m]);
  }
  // Oldest first, so a member does not move because a sibling was relabelled.
  for (const level of kids.values()) {
    level.sort((x, y) => x.at - y.at || x.id.localeCompare(y.id));
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
   * pass that rescues unreachable members cannot tell "hidden on purpose" from
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
    member: Member | null,
    id: string,
    depth: number,
    parent: string | null,
  ): Subtree => {
    // Filtered against `reached` rather than trusted: this walks a journal, and
    // a cycle must not put a member in two places.
    const children = (kids.get(bucket(id)) ?? []).filter((c) => !reached.has(c.id));
    for (const c of children) reached.add(c.id);

    const folded = children.length > 0 && collapsed.includes(id);
    if (folded) for (const c of children) swallow(c.id);
    const below = folded ? [] : children.map((c) => place(c, c.id, depth + 1, id));

    // A leaf — or a folded member, which is a leaf as far as the picture goes —
    // takes the next row. Everything else centres on the children it spans.
    const first = below[0]?.node.lane;
    const last = below[below.length - 1]?.node.lane;
    const lane =
      first === undefined || last === undefined ? nextLane++ : (first + last) / 2;

    return {
      node: {
        id,
        // The main agent is named for what it is. It is the session, and a
        // session already has a title at the top of the page.
        label: member?.label ?? "main agent",
        status: member?.status ?? main?.status ?? "idle",
        agentType: member?.agentType ?? null,
        detail:
          member?.detail ??
          (main ? describeAgent(main.status, main.spawnedAtMs, main.endedAtMs) : "idle"),
        kind: member?.kind ?? "main",
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

  const root = place(null, mainId, 0, null);

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
  // agent, because a member nobody can find is worse than one drawn in the
  // wrong place.
  for (const m of members) {
    if (reached.has(m.id)) continue;
    reached.add(m.id);
    nodes.push({
      id: m.id,
      label: m.label,
      status: m.status,
      agentType: m.agentType,
      detail: m.detail,
      kind: m.kind,
      depth: 1,
      lane: nextLane++,
      parent: mainId,
      children: 0,
      descendants: 0,
      collapsed: false,
    });
    edges.push({ from: mainId, to: m.id });
  }

  const drawn = new Set(nodes.map((n) => n.id));
  return {
    nodes,
    edges,
    depth: nodes.reduce((d, n) => Math.max(d, n.depth + 1), 0),
    rows: Math.max(nextLane, 1),
    hidden: members.filter((m) => !drawn.has(m.id)).length,
  };
}

/**
 * How many members sit below each one, memoised.
 *
 * Counted over both rosters rather than over what is drawn: the number is
 * what a folded node reports, so folding must not change it.
 */
function countDescendants(
  kids: Map<string, Member[]>,
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
