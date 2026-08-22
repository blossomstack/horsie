import { MessageSquareText } from "lucide-react";
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

/** Where a lane sits and what a rank is worth, in pixels.
 *
 * Wider and taller than it was, because the name is what a node is *for* and
 * the name was the thing being cut: a title now gets the top two lines to
 * itself, and everything the reader can recover elsewhere — what kind of thing
 * this is, what became of it, which preset it runs — shares the third. */
const NODE_W = 212;
const NODE_H = 66;
/** Between ranks: wide enough that an edge reads as a run, not a joint. */
const GAP_X = 76;
/** One lane, node included. */
const ROW_H = 74;
const PAD = 20;
/** The fold control, straddling the edge its children come out of. */
const TOGGLE_R = 8;
/** The jump key: a square keycap in the node's top-right corner, the size the
 * console's icon keys are everywhere else. */
const JUMP_W = 20;
/** The glyph inside it, which is the transcript's own icon at key size. */
const JUMP_ICON = 13;

/** A node is a panel key lit by the same lamps the rest of the console uses:
 * live for work in motion, ok for an agent that landed, red for a fault.
 *
 * Fill and stroke are two maps rather than one string because the current run
 * takes the fill over — and two `fill-*` utilities in one class list do not
 * resolve by the order they were written in, they resolve by whichever
 * Tailwind emitted last. Picking one is the only way to be sure which wins. */
const STATUS_STROKE: Record<string, string> = {
  running: "stroke-live",
  provisioning: "stroke-live",
  awaiting_input: "stroke-accent",
  completed: "stroke-lamp-ok",
  failed: "stroke-red",
  cancelled: "stroke-rule-strong",
  idle: "stroke-rule",
};

const STATUS_FILL: Record<string, string> = {
  running: "fill-live-quiet",
  provisioning: "fill-live-quiet",
  awaiting_input: "fill-accent-quiet",
  completed: "fill-raised",
  failed: "fill-red-quiet",
  cancelled: "fill-raised",
  idle: "fill-raised",
};

/** Measured off the rendered graph, at the sizes the two lines are drawn in.
 * The first line of a name stops short of the jump key; the second has the
 * whole width, because the key is only ever on the first. */
const NAME_MAX_1 = 25;
const NAME_MAX_2 = 30;
const DETAIL_MAX = 38;
/** One character of the `+3` a fold carries, at 10px. */
const BADGE_CHAR_W = 6;

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/**
 * A name across two lines, broken at a word where there is one.
 *
 * Measured in characters rather than laid out, which is the trade SVG asks
 * for: there is no wrapping in `<text>`, and the alternative is a
 * `foreignObject` per node. A proportional font makes the count approximate,
 * so both budgets are set from what actually fitted on screen rather than from
 * arithmetic, and the hover title carries the whole name either way.
 */
function twoLines(text: string): [string, string?] {
  if (text.length <= NAME_MAX_1) return [text];
  const space = text.lastIndexOf(" ", NAME_MAX_1);
  // A break more than halfway along is a word break worth taking; anything
  // earlier would leave a stub of a first line, so the cut is mid-word.
  const head = space > NAME_MAX_1 / 2 ? text.slice(0, space) : text.slice(0, NAME_MAX_1);
  return [head, clip(text.slice(head.length).trimStart(), NAME_MAX_2)];
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
  /** The agent the panel is showing, if one is selected. Drawn as a ring
   *  *outside* the node: it is a passing choice — the next click moves it —
   *  and it has to be legible on top of both of the other two things a node
   *  says at once, its status and whether it is the run being read. */
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
          const isCurrent = current === n.id;
          const name = twoLines(n.label);
          // Every node can be inspected, the main agent included: the panel
          // answers for it too, and a picture in which one node does nothing
          // when you click it teaches that clicking does nothing. Its jump key
          // is the same rule one step on — every node is a run, and every run
          // has a transcript.
          return (
            <g key={n.id} transform={`translate(${x} ${y})`}>
              {/* What the panel is showing. A ring around the node rather
                  than the node's own border, which is spent on status: a
                  selected failed agent has to be able to read as both, and
                  overwriting the border made it read as neither. */}
              {selected === n.id && (
                <rect
                  x={-3.5}
                  y={-3.5}
                  width={NODE_W + 7}
                  height={NODE_H + 7}
                  rx={11}
                  data-testid={`agent-selected-${n.id}`}
                  className="fill-none stroke-legend stroke-[1.5]"
                />
              )}
              <g
                data-testid={`agent-node-${n.id}`}
                data-status={n.status}
                data-kind={n.kind}
                data-collapsed={n.collapsed ? "true" : undefined}
                data-current={isCurrent ? "true" : undefined}
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
                <title>
                  {`${n.label} — ${KIND_LABEL[n.kind]}, ${n.detail}`}
                  {isCurrent ? " · the run you are reading" : ""}
                </title>
                {/* One card, always.
                    A sub session used to get a second card offset behind the
                    first, to say "this is a session, not something delegated".
                    It could not: an offset rectangle shows its fill along two
                    edges only, so the node read as a card whose border had
                    slipped off its background rather than as a stack — and the
                    thing it was straining to communicate is now simply written
                    on the node.

                    The run this page is reading takes the console's selected
                    surface — the same fill a picked row has in the rail and in
                    every table. It used to be a dot at the left edge, three
                    pixels across, on a card two hundred wide: the one thing
                    the picture most needed to say was the quietest mark on it.
                    The status keeps the border and keeps its own line of
                    words, so nothing is lost by lending it the fill. */}
                <rect
                  width={NODE_W}
                  height={NODE_H}
                  rx={8}
                  className={cn(
                    "stroke-[1.5]",
                    STATUS_STROKE[n.status] ?? "stroke-rule",
                    isCurrent ? "fill-accent-quiet" : (STATUS_FILL[n.status] ?? "fill-raised"),
                  )}
                />
                {/* What kind of thing this is, above its name. A reader has to
                    be able to tell a sub session from a subagent — one owes a
                    result and one is a conversation — and every other cue for
                    it was either a shape or a colour already spent on status. */}
                {/* The name, over as many of the two lines as it needs. A
                    one-line name sits where a two-line one's first line would
                    have been, rather than centred: the meta line is anchored
                    to the bottom, and a name that moved up and down with its
                    own length made a row of nodes read as ragged. */}
                <text x={12} y={24} className="fill-legend text-[12px] font-medium">
                  {name[0]}
                </text>
                {name[1] && (
                  <text x={12} y={40} className="fill-legend text-[12px] font-medium">
                    {name[1]}
                  </text>
                )}
                {/* What kind of thing this is, what became of it, and which
                    preset it runs — one line, in that order.
                    The kind used to be a line of its own above the name, set
                    in the legend's upper case: a whole line, at the top of the
                    card, spent on the least specific thing a node has to say.
                    The status is in words as well as in colour here for the
                    same reason it always was — a lamp nobody can name is a
                    lamp only its author can read. */}
                <text x={12} y={NODE_H - 12} className="fill-dim text-[10px]">
                  {clip(
                    [KIND_LABEL[n.kind], n.status.replace(/_/g, " "), n.agentType]
                      .filter(Boolean)
                      .join(" · "),
                    DETAIL_MAX,
                  )}
                </text>
              </g>

              {/* Straight to that agent's transcript, without going through
                  the panel. Its own <g>, outside the one that selects the
                  node: nesting two activatable elements makes one of them
                  unreachable by keyboard.
                  On every node, the main agent included. It used to be left
                  off the root because "its transcript is the page this is
                  drawn on" — which is only true of the session's own page, and
                  is not what the key does anyway: it takes you to a transcript,
                  and from the graph that is a move whichever node you press.
                  A run scoped page had no way back to the session at all. */}
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
                className="group/jump cursor-pointer focus:outline-none"
              >
                <title>{`Open ${n.label}'s transcript`}</title>
                {/* An icon key, drawn the way every other icon key in the
                    console is: nothing at rest, a cap under the pointer.
                    `screen` rather than `raised`, which is what an icon key
                    lifts to everywhere else: a node's own fill is already
                    `raised` on three of the six statuses, and a cap the same
                    colour as the card it sits on is not a cap. It was a filled white disc with a border and no
                    hover — a control in a style the rest of the interface had
                    stopped using, sitting on top of a card whose own fill it
                    punched a hole in. */}
                <rect
                  x={NODE_W - JUMP_W - 4}
                  y={4}
                  width={JUMP_W}
                  height={JUMP_W}
                  rx={6}
                  // `fill-transparent`, never `fill-none`: an unpainted
                  // shape takes no pointer events, so the cap would only
                  // light up — and only be clickable — over the glyph itself.
                  className="fill-transparent group-hover/jump:fill-screen"
                />
                {/* The transcript's own glyph, the one on the view switch two
                    inches above. An arrow says "away"; this says where. */}
                <MessageSquareText
                  x={NODE_W - JUMP_W - 4 + (JUMP_W - JUMP_ICON) / 2}
                  y={4 + (JUMP_W - JUMP_ICON) / 2}
                  width={JUMP_ICON}
                  height={JUMP_ICON}
                  className="pointer-events-none text-faint group-hover/jump:text-legend"
                  aria-hidden
                />
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
