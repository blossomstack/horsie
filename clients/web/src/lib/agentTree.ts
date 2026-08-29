import { MAIN_AGENT } from "../api/client";
import type { SubAgentView, SubSessionView } from "../api/types";
import { clockTime, humanDuration } from "./format";
import { i18n } from "../i18n";

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
  /** Its own title, else the preset it runs, else what it is. */
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
  kind: AgentKind;
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

/** Statuses that mean the agent has not stopped, so it has no duration yet.
 *
 * Exported because three views ask the same question of the same word — the
 * graph, the timeline and the agent panel — and a fourth list of live statuses
 * is a fourth chance to disagree about whether `awaiting_input` is running. */
const LIVE_STATUS = new Set(["running", "provisioning", "awaiting_input"]);

export function isLive(status: string): boolean {
  return LIVE_STATUS.has(status);
}

/** What a drawn node is.
 *
 * Four, because a session hosts four different things and a reader has to be
 * able to tell them apart: only two owe a result, only one *is* the session,
 * and only one is itself a session. The server says which on every roster
 * entry, so this is read rather than inferred from which fields are set.
 */
export type AgentKind = "main" | "subagent" | "step" | "sub_session" | "run";

/** What each kind is called, where a picture has room to say so. */
/** A kind's name, looked up when it is drawn rather than captured here: a
 * module-level map is built once at import, so its words would stay in the
 * language the tab was opened in. */
export function kindLabel(kind: AgentKind): string {
  return i18n.t(`agentKind.${kind}`);
}

/**
 * A workflow run, drawn as the node its steps hang off.
 *
 * A run is not an agent — it has no transcript, no context and no id in the
 * agent space — so it is not on the roster and cannot be. Its node is
 * synthesised here, keyed by the run's own id so a session hosting several
 * gets several: its own, if it is a run, and one per `invoke_workflow` any of
 * its agents called. Prefixed rather than bare so it can never collide with an
 * agent id, and stable across reloads so a fold or a selection naming one
 * survives.
 */
export const RUN_PREFIX = "run:";

export function runNodeId(runId: string): string {
  return `${RUN_PREFIX}${runId}`;
}

export function isRunNode(id: string): boolean {
  return id.startsWith(RUN_PREFIX);
}

/** One run a session hosts, with its executions in log order. */
export interface RunGroup {
  /** The run's own id, as the roster reports it. */
  id: string;
  /** What its node is drawn under. */
  nodeId: string;
  /** What it is called: the session's title for the run a session *is*, the
   * workflow's name for one an agent invoked. */
  label: string;
  /** The agent that invoked it; absent means the session's primary agent —
   * and for the run a session *is*, there is nobody to name. */
  parent?: string;
  /** Whether this is the session's own run, rather than one invoked inside it.
   * True only when the session has no main agent: a run *is* its steps. */
  root: boolean;
  /**
   * The executions, in the order the run log holds them.
   *
   * A run is a sequence: one step at a time, each handed the last one's
   * result. A step reached twice — by a loop or a retry — is two executions
   * and two entries here, which is what makes the log a chain rather than a
   * set.
   */
  steps: SubAgentView[];
}

/**
 * Every run a session hosts, grouped from its roster.
 *
 * Grouped by the run each execution names, not by "is a step": a session can
 * host several runs at once, and flattened into one list they draw as a single
 * impossible run whose steps interleave.
 */
export function runGroups(agents: SubAgentView[], rootTitle?: string): RunGroup[] {
  // A session with a main agent is not a run — it is a session that invoked
  // one. The presence of steps says nothing either way.
  const isRun = !agents.some((a) => a.kind === "main");
  const byRun = new Map<string, SubAgentView[]>();
  for (const a of agents) {
    if (a.kind !== "step" || !a.run) continue;
    byRun.set(a.run, [...(byRun.get(a.run) ?? []), a]);
  }
  return [...byRun.entries()]
    .map(([id, steps]) => {
      const ordered = [...steps].sort(
        (x, y) => x.spawnedAtMs - y.spawnedAtMs || x.id.localeCompare(y.id),
      );
      const parent = ordered[0]?.parent;
      const root = isRun && parent === undefined;
      return {
        id,
        nodeId: runNodeId(id),
        label: (root ? rootTitle : undefined) ?? ordered[0]?.workflow ?? i18n.t("agentKind.run"),
        parent,
        root,
        steps: ordered,
      };
    })
    .sort((x, y) => (x.steps[0]?.spawnedAtMs ?? 0) - (y.steps[0]?.spawnedAtMs ?? 0));
}

/** The executions of one run, in log order. */
export function stepRuns(agents: SubAgentView[], run?: string): SubAgentView[] {
  return agents
    .filter((a) => a.kind === "step" && (run === undefined || a.run === run))
    .sort((x, y) => x.spawnedAtMs - y.spawnedAtMs || x.id.localeCompare(y.id));
}

/** What the run is doing, folded from its steps.
 *
 * A run has no status of its own on the roster — the session's own status is
 * in a different vocabulary, one where a session is `Finished` and an agent is
 * `completed` — so it is read off the steps, which speak the vocabulary both
 * pictures paint in. A fault outranks a finish: a run whose middle step failed
 * and whose last one landed has still failed. */
export function runStatus(steps: SubAgentView[]): string {
  const live = steps.find((s) => isLive(s.status));
  if (live) return live.status;
  if (steps.some((s) => s.status === "failed")) return "failed";
  if (steps.some((s) => s.status === "cancelled")) return "cancelled";
  return steps.length > 0 && steps.every((s) => s.status === "completed") ? "completed" : "idle";
}

/** What the run is doing, how long it has taken so far, and when it began —
 * folded from its steps, the way an agent's own detail is folded from its two
 * stamps. */
export function describeRun(steps: SubAgentView[]): string {
  const at = steps[0]?.spawnedAtMs ?? 0;
  const ended = steps.reduce((last, s) => Math.max(last, s.endedAtMs), 0);
  return describeAgent(runStatus(steps), at, ended);
}

/** The run as the one shape both walks read. */
function runMember(group: RunGroup): Member {
  return {
    id: group.nodeId,
    parent: group.parent,
    label: group.label,
    status: runStatus(group.steps),
    agentType: null,
    detail: describeRun(group.steps),
    kind: "run",
    at: group.steps[0]?.spawnedAtMs ?? 0,
  };
}

/**
 * The runs a roster implies, and where each step hangs once they are drawn.
 *
 * `chain` is the one difference between the two pictures. The graph draws the
 * log as the sequence it is — each execution hanging off the one before it —
 * because that is what an edge can say there. The timeline lays its lanes on a
 * time axis, where the order is already visible along the axis, so nesting
 * each step under the last would only spend indentation saying it twice.
 */
function runShape(
  agents: SubAgentView[],
  chain: boolean,
  rootTitle?: string,
): { members: Member[]; parentOf: Map<string, string>; rootNodeId?: string } {
  const groups = runGroups(agents, rootTitle);
  const parentOf = new Map<string, string>();
  for (const group of groups) {
    group.steps.forEach((step, i) => {
      parentOf.set(step.id, chain && i > 0 ? group.steps[i - 1].id : group.nodeId);
    });
  }
  return {
    members: groups.map(runMember),
    parentOf,
    rootNodeId: groups.find((g) => g.root)?.nodeId,
  };
}

/** One drawable thing, from either roster, in the one shape the walk reads. */
interface Member {
  id: string;
  parent: string | undefined;
  label: string;
  status: string;
  agentType: string | null;
  detail: string;
  kind: AgentKind;
  /** When it came into being — the order siblings are drawn in. */
  at: number;
}

/**
 * Where a member sits among its siblings: delegated work first, then the
 * sessions branched off.
 *
 * Two kinds of thing hang off one agent and they are not the same kind of
 * thing — a subagent or a step is work inside a turn, and a sub session is
 * another session, talked to rather than delegated to. Ordered purely by when
 * they started they interleaved, and the reader had to do the sorting. The
 * timeline used to say it with a rule drawn across the lanes instead; grouping
 * says it without spending a row on it, and the graph gets it for free.
 *
 * Oldest first inside each group, and by id when two share a stamp, so nothing
 * moves because a sibling was relabelled.
 */
const SIBLING_ORDER: Record<AgentKind, number> = {
  main: 0,
  subagent: 0,
  run: 0,
  sub_session: 1,
  // Last, and only ever reached in the graph, where the run is drawn as a
  // chain: a step under another step is what the run did *next*, so it belongs
  // below everything that step delegated rather than among it.
  step: 2,
};

function bySibling(x: Member, y: Member): number {
  return (
    SIBLING_ORDER[x.kind] - SIBLING_ORDER[y.kind] ||
    x.at - y.at ||
    x.id.localeCompare(y.id)
  );
}

function agentMember(a: SubAgentView): Member {
  return {
    id: a.id,
    parent: a.parent,
    label: a.title ?? a.agentType ?? "subagent",
    status: a.status,
    agentType: a.agentType ?? null,
    detail: describeAgent(a.status, a.spawnedAtMs, a.endedAtMs),
    // Read, never guessed: a workflow step reaches this roster too, and it is
    // not a subagent — the definition chose it, no agent delegated to it.
    kind: a.kind === "step" ? "step" : "subagent",
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
    label: s.title,
    status: s.status,
    agentType: null,
    detail: describeAgent(s.status, s.createdAtMs, s.lastActivityMs),
    kind: "sub_session",
    at: s.createdAtMs,
  };
}

/** One member of the hosted tree, in the order and at the depth it is drawn. */
export interface HostedMember {
  id: string;
  parent: string | null;
  label: string;
  status: string;
  agentType: string | null;
  detail: string;
  kind: AgentKind;
  /** Distance from the root, in edges. Anything hanging off the root is 1. */
  depth: number;
  /** When it came into being. */
  at: number;
  /** How many members hang directly off it. */
  children: number;
  /** Every member below it, folded or not. */
  descendants: number;
}

/**
 * Everything a session hosts below `rootId`, in render order.
 *
 * The one place the nesting is decided, because there are two pictures of it
 * and they were deriving it separately: the timeline built its own map keyed
 * only by the *agent* roster, so a subagent whose parent was a sub session
 * found no such parent and fell into the main agent's bucket — drawn beside
 * the sub session that spawned it, as though the session had delegated to it
 * directly. The graph got that right; only one of the two could be.
 *
 * `rootId` is what the tree is drawn *from*, which is not always the main
 * agent: a page scoped to one agent shows that agent's own work, and the
 * members below it are the only ones that belong on it.
 */
export function hostedTree(
  agents: SubAgentView[],
  subSessions: SubSessionView[],
  rootId: string,
  collapsed: readonly string[] = [],
  /**
   * Whether a member nobody can place belongs on this root.
   *
   * True when the root is the session itself: an agent whose parent is missing
   * is better drawn in the wrong place than not drawn at all, and the session
   * is the only honest place left. False when the root is one agent inside the
   * session — the tree is then that agent's own subtree, and an orphan is not
   * its work. Without this the session's *main* agent, whose parent is
   * nobody, was swept into a subagent's bucket and drawn as its child.
   */
  rescueOrphans = true,
): HostedMember[] {
  // Runs are drawn here too, as a member with its executions under it — flat,
  // not chained: the lanes already run left to right along a time axis, and
  // nesting each step under the last would spend indentation saying it twice.
  const shape = runShape(agents, false);
  const members: Member[] = [
    ...agents.filter((a) => a.id !== rootId).map(agentMember),
    ...subSessions.filter((s) => s.id !== rootId).map(subSessionMember),
    ...shape.members.filter((m) => m.id !== rootId),
  ].map((m) => ({ ...m, parent: shape.parentOf.get(m.id) ?? m.parent }));
  const held = new Set(members.map((m) => m.id));
  const kids = new Map<string, Member[]>();
  for (const m of members) {
    const linked = m.parent && m.parent !== m.id && held.has(m.parent);
    if (!linked && m.parent !== rootId && !rescueOrphans) continue;
    const key = linked && m.parent !== rootId ? (m.parent ?? "") : "";
    kids.set(key, [...(kids.get(key) ?? []), m]);
  }
  for (const level of kids.values()) level.sort(bySibling);
  const bucket = (id: string) => (id === rootId ? "" : id);
  const descendantsOf = countDescendants(kids, bucket);

  const out: HostedMember[] = [];
  const reached = new Set<string>([rootId]);
  const swallow = (id: string) => {
    for (const child of kids.get(bucket(id)) ?? []) {
      if (reached.has(child.id)) continue;
      reached.add(child.id);
      swallow(child.id);
    }
  };
  const walk = (id: string, depth: number) => {
    const children = (kids.get(bucket(id)) ?? []).filter((c) => !reached.has(c.id));
    for (const c of children) reached.add(c.id);
    // A fold hides the whole subtree, not the row below it: an unmarked
    // grandchild comes back through the rescue pass as an orphan on the root.
    if (children.length > 0 && collapsed.includes(id)) {
      for (const c of children) swallow(c.id);
      return;
    }
    for (const c of children) {
      out.push({
        ...c,
        parent: id === rootId ? rootId : id,
        depth,
        children: (kids.get(c.id) ?? []).length,
        descendants: descendantsOf(c.id),
      });
      walk(c.id, depth + 1);
    }
  };
  // Depth 1, not 0: everything here hangs off the root, and a child drawn at
  // the root's own depth is a child no fold can hide — the collapse walk reads
  // depth, and nothing was ever deeper than the lane it was under.
  walk(rootId, 1);

  // Only reachable if the roster is not a tree.
  if (rescueOrphans) {
    for (const m of members) {
      if (reached.has(m.id)) continue;
      reached.add(m.id);
      out.push({ ...m, parent: rootId, depth: 1, children: 0, descendants: 0 });
    }
  }
  return out;
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
  /**
   * What this run is called, when the session *is* a run — its own title,
   * which defaults to the workflow's name and can be renamed like any
   * session's.
   *
   * Its absence is the gate, and steps in the roster are not: an agent that
   * calls `invoke_workflow` starts a run inside an ordinary session, and the
   * roster lists those executions too — "the session's own and any invoked
   * one's". Keyed off the steps, such a session would have been rooted on a
   * run node with its own main agent swept inside it.
   */
  runTitle?: string,
): AgentTree {
  if (agents.length === 0 && subSessions.length === 0) {
    return { nodes: [], edges: [], depth: 0, rows: 0, hidden: 0 };
  }

  // Every run the session hosts, and where each execution hangs once they are
  // drawn: under its run, then one after another, which is the sequence a run
  // log is.
  const shape = runShape(agents, true, runTitle);

  // The main agent is the one nothing spawned. The same fallbacks the timeline
  // uses, so the two views agree on which agent is the root. A session that
  // *is* a run has no such agent, and that run's node stands in its place.
  const main = agents.find((a) => !a.parent && a.depth === 0) ?? agents[0];
  const mainId = shape.rootNodeId ?? (main?.id ?? MAIN_AGENT);
  const rootRun = shape.members.find((m) => m.id === mainId);

  const members: Member[] = [
    ...agents.filter((a) => a.id !== mainId).map(agentMember),
    ...subSessions.map(subSessionMember),
    ...shape.members.filter((m) => m.id !== mainId),
  ].map((m) => ({ ...m, parent: shape.parentOf.get(m.id) ?? m.parent }));

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
  for (const level of kids.values()) level.sort(bySibling);
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
        // The main agent carries the session's title, because that title *is*
        // its own: the session is named by naming its main agent. It used to
        // read "main agent" — the one node in the picture that said what it
        // was instead of what it was doing.
        label: member?.label ?? main?.title ?? i18n.t("agentKind.mainAgent"),
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

  const root = place(rootRun ?? null, mainId, 0, null);

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
    parts.push(humanDuration(endMs - startMs));
  }
  if (startMs > 0) parts.push(`started ${clockTime(startMs)}`);
  return parts.join(" · ");
}

