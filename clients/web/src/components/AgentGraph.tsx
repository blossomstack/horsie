import type { AgentTree, PlacedAgent } from "../lib/agentTree";
import { KIND_LABEL } from "../lib/agentTree";
import { cn } from "../lib/cn";

/**
 * A session's agents, drawn as the tree they actually are.
 *
 * SVG rather than positioned divs, which is the opposite of what
 * `SessionTimeline` chose and for the reason that component states: a timeline
 * has nothing to route, and this does. An agent hanging off another is a claim
 * about lineage, and the only honest way to draw a claim about lineage is a
 * line from one to the other.
 *
 * The tree flows left to right — depth on x — because an agent's name is the
 * widest thing on it, and a name is far easier to fit in a row than in a
 * column. It is also the axis `WorkflowGraph` already grows along, so the two
 * structural views of a session read the same way.
 */

/** Where a lane sits and what a rank is worth, in pixels. */
const NODE_W = 196;
/** Three lines now — kind, title, status — so the card is taller. */
const NODE_H = 58;
/** Between ranks: wide enough that an edge reads as a run, not a joint. */
const GAP_X = 76;
/** One lane, node included. */
const ROW_H = 74;
const PAD = 20;
/** The fold control, straddling the edge its children come out of. */
const TOGGLE_R = 8;
/** The jump key, inset from the node's top-right corner. */
const JUMP_R = 9;

/** A node is a panel key lit by the same lamps the rest of the console uses:
 * live for work in motion, ok for an agent that landed, red for a fault. */
const STATUS_CLASS: Record<string, string> = {
  running: "fill-live-quiet stroke-live",
  provisioning: "fill-live-quiet stroke-live",
  awaiting_input: "fill-accent-quiet stroke-accent",
  completed: "fill-raised stroke-lamp-ok",
  failed: "fill-red-quiet stroke-red",
  cancelled: "fill-raised stroke-rule-strong",
  idle: "fill-raised stroke-rule",
};

/** Measured off the rendered graph. The name is cut short of the jump key. */
const NAME_MAX = 22;
const DETAIL_MAX = 26;
/** One character of the `+3` a fold carries, at 10px. */
const BADGE_CHAR_W = 6;

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

export function AgentGraph({
  tree,
  selected,
  current,
  onToggleCollapse,
  onSelectAgent,
  onOpenAgent,
}: {
  tree: AgentTree;
  /** The agent the panel is showing, if one is selected. */
  selected?: string;
  /** The agent whose transcript this page is scoped to, if any. "You are
   *  here": the three views are three readings of one session, and switching
   *  to a structural view from a subagent's page used to lose all trace of
   *  which run you had been reading. */
  current?: string;
  /** Fold or unfold everything hanging off one agent. */
  onToggleCollapse: (agentId: string) => void;
  /** Show one agent in the panel beside the graph. Every node, the main agent
   *  included — the panel answers for all of them. */
  onSelectAgent: (agentId: string) => void;
  /** Leave the graph for one agent's transcript. The jump key only. */
  onOpenAgent: (agentId: string) => void;
}) {
  if (tree.nodes.length === 0) {
    return (
      <div
        className="flex h-full items-center justify-center px-6"
        data-testid="agent-graph-empty"
      >
        <p className="max-w-sm text-center text-sm leading-relaxed text-dim">
          No agents have been recorded for this session yet. The graph draws
          itself as they start.
        </p>
      </div>
    );
  }

  const at = (n: PlacedAgent) => ({
    x: PAD + n.depth * (NODE_W + GAP_X),
    y: PAD + n.lane * ROW_H,
  });
  const placed = new Map(tree.nodes.map((n) => [n.id, n]));

  // Measured off the nodes rather than off the rank count: a fold hangs its
  // control and its count past the right edge of the node, and a canvas sized
  // to the ranks alone cut the count in half.
  const width =
    PAD +
    tree.nodes.reduce((right, n) => {
      const badge = n.collapsed ? 6 + `+${n.descendants}`.length * BADGE_CHAR_W : 0;
      const toggle = n.children > 0 ? TOGGLE_R : 0;
      return Math.max(right, at(n).x + NODE_W + toggle + badge);
    }, 0);
  const height = PAD * 2 + (tree.rows - 1) * ROW_H + NODE_H;

  // Out of the parent's right edge, into the child's left edge. Flat-tangent
  // curves rather than elbows: at this rank spacing a fan of six children is
  // six near-identical right angles, and a curve says which one belongs to
  // which child at a glance.
  const wires = tree.edges.flatMap((e) => {
    const from = placed.get(e.from);
    const to = placed.get(e.to);
    if (!from || !to) return [];
    const a = at(from);
    const b = at(to);
    const x1 = a.x + NODE_W;
    const y1 = a.y + NODE_H / 2;
    const x2 = b.x;
    const y2 = b.y + NODE_H / 2;
    return [
      {
        key: `${e.from}-${e.to}`,
        live: to.status === "running" || to.status === "provisioning",
        d: `M ${x1} ${y1} C ${x1 + GAP_X / 2} ${y1}, ${x2 - GAP_X / 2} ${y2}, ${x2} ${y2}`,
      },
    ];
  });

  return (
    // The ground comes from `SessionPane`, which all three session views
    // share. Centred both ways WHILE IT FITS: a two-node session drawn in the
    // top-left corner of a wide pane reads as a rendering that failed rather
    // than a session with two agents in it. `min-h-full` on the inner box is
    // what lets it centre and still scroll once the graph outgrows the pane.
    <div className="h-full overflow-auto" data-testid="agent-graph">
      <div className="flex min-h-full min-w-full items-center justify-center p-6">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width={width}
        height={height}
        // A group, not an image. `role="img"` is what a static picture claims,
        // and it licenses assistive tech to prune everything inside — which
        // here is every node and every fold control.
        role="group"
        aria-label="Agent graph"
        // Drawn at its natural size and centred while it fits, rather than
        // stretched to the pane: an SVG given a width in CSS scales its
        // contents to match, so a two-node session would have been blown up to
        // fill a screen it never needed.
        className="mx-auto block"
      >
        {wires.map((w) => (
          <path
            key={w.key}
            d={w.d}
            fill="none"
            className={cn(
              "stroke-[1.5]",
              w.live ? "stroke-live" : "stroke-rule-strong opacity-70",
            )}
          />
        ))}

        {tree.nodes.map((n) => {
          const { x, y } = at(n);
          // Every node can be inspected, the main agent included: the panel
          // answers for it too, and a picture in which one node does nothing
          // when you click it teaches that clicking does nothing.
          const jumpable = n.kind !== "main";
          return (
            <g key={n.id} transform={`translate(${x} ${y})`}>
              <g
                data-testid={`agent-node-${n.id}`}
                data-status={n.status}
                data-kind={n.kind}
                data-collapsed={n.collapsed ? "true" : undefined}
                onClick={() => onSelectAgent(n.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onSelectAgent(n.id);
                  }
                }}
                tabIndex={0}
                role="button"
                aria-label={`Show ${n.label}`}
                aria-pressed={selected === n.id}
                className="cursor-pointer focus:outline-none"
              >
                {/* The whole of the name and what became of it. The node is a
                    fixed width, so both are cut on it; this is where the uncut
                    version lives. It names the kind as well, because that is
                    the thing a shape alone could never say out loud. */}
                <title>{`${n.label} — ${KIND_LABEL[n.kind]}, ${n.detail}`}</title>
                {/* One card, always.
                    A sub session used to get a second card offset behind the
                    first, to say "this is a session, not something delegated".
                    It could not: an offset rectangle shows its fill along two
                    edges only, so the node read as a card whose border had
                    slipped off its background rather than as a stack — and the
                    thing it was straining to communicate is now simply written
                    on the node. */}
                <rect
                  width={NODE_W}
                  height={NODE_H}
                  rx={8}
                  className={cn(
                    STATUS_CLASS[n.status] ?? "fill-raised stroke-rule",
                    "stroke-[1.5]",
                    selected === n.id && "stroke-legend stroke-[2.5]",
                  )}
                />
                {/* The run this page is on. A dot rather than a second border:
                    the border is already spoken for by status and by
                    selection, and a node can be all three at once. */}
                {current === n.id && (
                  <circle
                    cx={6}
                    cy={NODE_H / 2}
                    r={3}
                    className="fill-legend"
                    data-testid={`agent-current-${n.id}`}
                  >
                    <title>The run you are reading</title>
                  </circle>
                )}
                {/* What kind of thing this is, above its name. A reader has to
                    be able to tell a sub session from a subagent — one owes a
                    result and one is a conversation — and every other cue for
                    it was either a shape or a colour already spent on status. */}
                <text x={12} y={17} className="fill-faint text-[9px] tracking-[0.08em] uppercase">
                  {KIND_LABEL[n.kind]}
                </text>
                <text x={12} y={33} className="fill-legend text-[12px] font-medium">
                  {clip(n.label, NAME_MAX)}
                </text>
                {/* The status in words as well as in colour: a lamp nobody can
                    name is a lamp only its author can read. */}
                <text x={12} y={47} className="fill-dim text-[10px]">
                  {clip(
                    n.agentType
                      ? `${n.status.replace(/_/g, " ")} · ${n.agentType}`
                      : n.status.replace(/_/g, " "),
                    DETAIL_MAX,
                  )}
                </text>
              </g>

              {/* Straight to that agent's transcript, without going through the
                  panel. Its own <g>, outside the one that selects the node:
                  nesting two activatable elements makes one of them
                  unreachable by keyboard. Not drawn on the main agent, whose
                  transcript is the page this is drawn on. */}
              {jumpable && (
                <g
                  data-testid={`agent-jump-${n.id}`}
                  onClick={(e) => {
                    // Or the node behind it selects at the same time, and the
                    // panel opens on the page you are leaving.
                    e.stopPropagation();
                    onOpenAgent(n.id);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      e.stopPropagation();
                      onOpenAgent(n.id);
                    }
                  }}
                  tabIndex={0}
                  role="button"
                  aria-label={`Open ${n.label}'s transcript`}
                  className="cursor-pointer focus:outline-none"
                >
                  <title>{`Open ${n.label}'s transcript`}</title>
                  <circle
                    cx={NODE_W - JUMP_R - 5}
                    cy={JUMP_R + 5}
                    r={JUMP_R}
                    className="fill-panel stroke-rule stroke-[1]"
                  />
                  {/* An arrow leaving a corner: the "open this elsewhere"
                      glyph the rest of the console uses. */}
                  <path
                    d={`M ${NODE_W - JUMP_R - 8} ${JUMP_R + 8} L ${NODE_W - JUMP_R - 1} ${JUMP_R + 1}
                        M ${NODE_W - JUMP_R - 4.5} ${JUMP_R + 1} L ${NODE_W - JUMP_R - 1} ${JUMP_R + 1}
                        L ${NODE_W - JUMP_R - 1} ${JUMP_R + 4.5}`}
                    fill="none"
                    className="stroke-dim stroke-[1.5]"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  />
                </g>
              )}

              {/* The fold sits on the edge the children leave by, so what it
                  discloses is the thing it is pointing at. Its own <g>, outside
                  the one that opens the agent: nesting two activatable elements
                  makes one of them unreachable by keyboard. */}
              {n.children > 0 && (
                <g
                  data-testid={`agent-collapse-${n.id}`}
                  onClick={() => onToggleCollapse(n.id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      onToggleCollapse(n.id);
                    }
                  }}
                  tabIndex={0}
                  role="button"
                  aria-expanded={!n.collapsed}
                  aria-label={`${n.collapsed ? "Show" : "Hide"} the ${
                    n.descendants === 1 ? "agent" : `${n.descendants} agents`
                  } under ${n.label}`}
                  className="cursor-pointer focus:outline-none"
                >
                  <title>
                    {n.collapsed
                      ? `Show what ${n.label} spawned — ${n.descendants} hidden`
                      : `Hide what ${n.label} spawned`}
                  </title>
                  <circle
                    cx={NODE_W}
                    cy={NODE_H / 2}
                    r={TOGGLE_R}
                    className="fill-panel stroke-rule-strong stroke-[1.5]"
                  />
                  <path
                    d={`M ${NODE_W - 4} ${NODE_H / 2} H ${NODE_W + 4}`}
                    className="stroke-legend stroke-[1.5]"
                  />
                  {n.collapsed && (
                    <path
                      d={`M ${NODE_W} ${NODE_H / 2 - 4} V ${NODE_H / 2 + 4}`}
                      className="stroke-legend stroke-[1.5]"
                    />
                  )}
                </g>
              )}

              {/* What a fold stands for. Drawn only when folded: expanded, the
                  count is the picture itself. */}
              {n.collapsed && (
                <text
                  x={NODE_W + TOGGLE_R + 6}
                  y={NODE_H / 2 + 4}
                  data-testid={`agent-hidden-${n.id}`}
                  className="fill-faint text-[10px]"
                >
                  +{n.descendants}
                </text>
              )}
            </g>
          );
        })}
      </svg>

      </div>
    </div>
  );
}
