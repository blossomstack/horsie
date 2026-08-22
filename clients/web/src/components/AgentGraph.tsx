import type { AgentTree, PlacedAgent } from "../lib/agentTree";
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
const NODE_W = 176;
const NODE_H = 46;
/** Between ranks: wide enough that an edge reads as a run, not a joint. */
const GAP_X = 76;
/** One lane, node included. */
const ROW_H = 62;
const PAD = 20;
/** The fold control, straddling the edge its children come out of. */
const TOGGLE_R = 8;
/** How far a sub session's second card sits behind the first. */
const CARD_OFFSET = 5;

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

/** Martian Mono at 12px, measured off the rendered graph. */
const NAME_MAX = 24;
const DETAIL_MAX = 28;
/** One character of the `+3` a fold carries, at 10px. */
const BADGE_CHAR_W = 6;

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

export function AgentGraph({
  tree,
  selected,
  onToggleCollapse,
  onSelectAgent,
}: {
  tree: AgentTree;
  /** The agent whose transcript is open, if this session is showing one. */
  selected?: string;
  /** Fold or unfold everything hanging off one agent. */
  onToggleCollapse: (agentId: string) => void;
  /** Open one agent's transcript. Not called for the main agent, whose
   *  transcript is the page this is drawn on. */
  onSelectAgent: (agentId: string) => void;
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
      // A sub session's second card sits `CARD_OFFSET` to the right of the
      // first, so it is part of the node's extent even when nothing else is.
      const card = n.kind === "sub_session" ? CARD_OFFSET : 0;
      return Math.max(right, at(n).x + NODE_W + Math.max(toggle + badge, card));
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
          const openable = n.kind !== "main";
          const branched = n.kind === "sub_session";
          return (
            <g key={n.id} transform={`translate(${x} ${y})`}>
              <g
                data-testid={`agent-node-${n.id}`}
                data-status={n.status}
                data-kind={n.kind}
                data-collapsed={n.collapsed ? "true" : undefined}
                onClick={openable ? () => onSelectAgent(n.id) : undefined}
                onKeyDown={
                  openable
                    ? (e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onSelectAgent(n.id);
                        }
                      }
                    : undefined
                }
                tabIndex={openable ? 0 : undefined}
                role={openable ? "button" : undefined}
                aria-label={openable ? `Open ${n.label}` : undefined}
                className={cn(openable && "cursor-pointer focus:outline-none")}
              >
                {/* The whole of the name and what became of it. The node is a
                    fixed width, so both are cut on it; this is where the uncut
                    version lives. It names the kind as well, because the one
                    thing the shape says is the one thing a screen reader
                    cannot see. */}
                <title>
                  {branched
                    ? `${n.label} — sub session, ${n.detail}`
                    : `${n.label} — ${n.detail}`}
                </title>
                {/* A second card behind the first: a sub session is a session,
                    not something this one delegated to, and the picture has to
                    say which it is drawing before the label is read. Offset up
                    and right so it never touches the edge an incoming wire
                    lands on. */}
                {branched && (
                  <rect
                    x={CARD_OFFSET}
                    y={-CARD_OFFSET}
                    width={NODE_W}
                    height={NODE_H}
                    rx={8}
                    className="fill-panel stroke-rule stroke-[1.5]"
                  />
                )}
                <rect
                  width={NODE_W}
                  height={NODE_H}
                  rx={8}
                  className={cn(
                    STATUS_CLASS[n.status] ?? "fill-raised stroke-rule",
                    "stroke-[1.5]",
                    selected === n.id && "stroke-live stroke-[2.5]",
                  )}
                />
                <text x={12} y={20} className="fill-legend text-[12px] font-medium">
                  {clip(n.label, NAME_MAX)}
                </text>
                {/* The status in words as well as in colour: a lamp nobody can
                    name is a lamp only its author can read. */}
                <text x={12} y={35} className="fill-dim text-[10px]">
                  {clip(
                    n.agentType
                      ? `${n.status.replace(/_/g, " ")} · ${n.agentType}`
                      : n.status.replace(/_/g, " "),
                    DETAIL_MAX,
                  )}
                </text>
              </g>

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
