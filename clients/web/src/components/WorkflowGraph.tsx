import { layoutGraph, type LayoutEdge, type LayoutNode } from "../lib/graphLayout";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";

/**
 * One renderer for both the editor's live preview and a run's execution graph.
 *
 * The editor passes a definition and no state; a run passes the same shape
 * plus a status per node and a highlight per edge. Built once because the two
 * views are the same picture with different paint.
 */

/** What a node looks like right now. `idle` is a step the run never reached. */
export type NodeState = "idle" | "running" | "done" | "failed" | "cancelled";

export interface GraphNode extends LayoutNode {
  state?: NodeState;
  /** Shown under the name — an agent preset, or an attempt count. */
  detail?: string;
  /** Number of executions, when more than one landed here. */
  attempts?: number;
}

export interface GraphEdge extends LayoutEdge {
  condition?: string | null;
  /** Whether any execution took this edge. */
  taken?: boolean;
}

interface Props {
  nodes: GraphNode[];
  edges: GraphEdge[];
  start: string;
  /** Called when a node is activated; omit to render a static picture. */
  onSelect?: (step: string) => void;
  selected?: string;
  className?: string;
}

/**
 * The graph flows left to right: rank on x, order within the rank on y.
 *
 * Horizontal because that is the axis a workflow grows along — steps chain far
 * more often than they fan out — and because a horizontal edge has room for its
 * condition above it, which a vertical one does not.
 */
const NODE_W = 168;
const NODE_H = 56;
/** Between ranks: wide enough for a condition label to sit on the edge. */
const GAP_X = 116;
/** Between siblings in one rank. */
const GAP_Y = 28;
const PAD = 16;
/** Martian Mono at 10px, `wdth 87.5`, measured off the rendered graph. */
const LABEL_CHAR_W = 6;
const LABEL_H = 14;
/** What fits in the gap between two ranks without touching either node. */
const LABEL_MAX = Math.floor((GAP_X - 10) / LABEL_CHAR_W);

/** A condition, cut to what the gap between two ranks can hold. */
function labelText(condition: string): string {
  return condition.length > LABEL_MAX
    ? `${condition.slice(0, LABEL_MAX - 1)}…`
    : condition;
}

/** A node reads as a panel key, lit by the same lamp colours the rest of the
 * console uses: live for work in motion, ok for a step that landed, red for a
 * fault. An unlit node is one the run has not reached. */
const STATE_CLASS: Record<NodeState, string> = {
  idle: "fill-raised stroke-rule",
  running: "fill-live-quiet stroke-live",
  done: "fill-raised stroke-lamp-ok",
  failed: "fill-red-quiet stroke-red",
  cancelled: "fill-raised stroke-rule-strong",
};

export function WorkflowGraph({
  nodes,
  edges,
  start,
  onSelect,
  selected,
  className,
}: Props) {
  const { t } = useTranslation();
  const layout = layoutGraph(nodes, edges, start);
  const byStep = new Map(nodes.map((n) => [n.step, n]));
  const placed = new Map(layout.nodes.map((n) => [n.step, n]));

  // Centre each rank on the cross axis, so a branch reads as a sub session rather
  // than as a row that starts at the top.
  const breadth = Math.max(layout.breadth, 1);
  const centreY = (rank: number) => {
    const count = layout.nodes.filter((n) => n.rank === rank).length;
    return ((breadth - count) * (NODE_H + GAP_Y)) / 2;
  };
  const position = (step: string) => {
    const p = placed.get(step);
    if (!p) return { x: 0, y: 0 };
    return {
      x: PAD + p.rank * (NODE_W + GAP_X),
      y: PAD + centreY(p.rank) + p.order * (NODE_H + GAP_Y),
    };
  };

  const width = PAD * 2 + layout.depth * NODE_W + Math.max(0, layout.depth - 1) * GAP_X;
  // A loop dips below the deepest row, and its condition sits below that, so
  // the canvas grows a lane for them rather than clipping.
  const backLane = layout.edges.some((e) => e.back) ? NODE_H * 0.7 + 12 : 0;
  const height = PAD * 2 + breadth * NODE_H + (breadth - 1) * GAP_Y + backLane;

  // Geometry once, so the labels can be drawn in their own pass on top of the
  // nodes: a condition painted under a node is a condition nobody can read.
  const drawn = layout.edges.map((e) => {
    const from = position(e.from);
    const to = position(e.to);
    const meta = edges.find((x) => x.from === e.from && x.to === e.to);
    // Forward: out of the right edge, into the left edge, label riding above
    // the middle of the run.
    const x1 = from.x + NODE_W;
    const y1 = from.y + NODE_H / 2;
    const x2 = to.x;
    const y2 = to.y + NODE_H / 2;
    // A back-edge dips under the row it returns across, so it cannot be
    // mistaken for forward progress. A self-loop has nowhere to travel, so it
    // becomes a small arc under its own node.
    const dip = NODE_H * 0.7;
    const self = e.from === e.to;
    const bx1 = self ? from.x + NODE_W * 0.35 : from.x + NODE_W / 2;
    const bx2 = self ? to.x + NODE_W * 0.65 : to.x + NODE_W / 2;
    const by1 = from.y + NODE_H;
    const by2 = to.y + NODE_H;
    return {
      from: e.from,
      to: e.to,
      back: e.back,
      taken: !!meta?.taken,
      condition: meta?.condition ? labelText(meta.condition) : null,
      fullCondition: meta?.condition ?? null,
      d: e.back
        ? `M ${bx1} ${by1} C ${bx1} ${by1 + dip}, ${bx2} ${by2 + dip}, ${bx2} ${by2}`
        : `M ${x1} ${y1} C ${x1 + GAP_X / 2} ${y1}, ${x2 - GAP_X / 2} ${y2}, ${x2} ${y2}`,
      labelX: e.back ? (bx1 + bx2) / 2 : (x1 + x2) / 2,
      labelY: e.back ? Math.max(by1, by2) + dip : (y1 + y2) / 2 - 8,
    };
  });

  if (layout.nodes.length === 0) {
    return (
      <p className={cn("text-sm text-faint", className)}>
        {t("workflowGraph.empty")}
      </p>
    );
  }

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width={width}
      height={height}
      className={cn("max-w-full", className)}
      role="img"
      aria-label={t("workflowGraph.ariaLabel")}
      data-testid="workflow-graph"
    >
      <defs>
        <marker
          id="wf-arrow"
          viewBox="0 0 8 8"
          refX="7"
          refY="4"
          markerWidth="7"
          markerHeight="7"
          orient="auto-start-reverse"
        >
          <path d="M 0 0 L 8 4 L 0 8 z" className="fill-rule-strong" />
        </marker>
      </defs>

      {drawn.map((e, i) => (
        <path
          key={`${e.from}-${e.to}-${i}`}
          d={e.d}
          fill="none"
          markerEnd="url(#wf-arrow)"
          className={cn(
            "stroke-[1.5]",
            e.taken ? "stroke-live" : "stroke-rule-strong opacity-60",
          )}
          strokeDasharray={e.back ? "4 3" : undefined}
        />
      ))}

      {layout.nodes.map((p) => {
        const node = byStep.get(p.step);
        const { x, y } = position(p.step);
        const state = node?.state ?? "idle";
        const interactive = !!onSelect;
        return (
          <g
            key={p.step}
            transform={`translate(${x} ${y})`}
            onClick={interactive ? () => onSelect(p.step) : undefined}
            onKeyDown={
              interactive
                ? (e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      onSelect(p.step);
                    }
                  }
                : undefined
            }
            tabIndex={interactive ? 0 : undefined}
            role={interactive ? "button" : undefined}
            aria-label={interactive ? `Step ${p.step}` : undefined}
            data-testid={`workflow-node-${p.step}`}
            data-state={state}
            className={cn(interactive && "cursor-pointer focus:outline-none")}
          >
            <rect
              width={NODE_W}
              height={NODE_H}
              rx={8}
              className={cn(
                STATE_CLASS[state],
                "stroke-[1.5]",
                selected === p.step && "stroke-live stroke-[2.5]",
                !p.reachable && "opacity-50",
              )}
            />
            <text x={12} y={22} className="fill-legend text-[13px] font-medium">
              {p.step.length > 20 ? `${p.step.slice(0, 19)}…` : p.step}
            </text>
            {node?.detail ? (
              <text x={12} y={40} className="fill-dim text-[11px]">
                {node.detail.length > 24 ? `${node.detail.slice(0, 23)}…` : node.detail}
              </text>
            ) : null}
            {node?.attempts && node.attempts > 1 ? (
              <>
                <circle cx={NODE_W - 16} cy={16} r={10} className="fill-panel stroke-rule" />
                <text
                  x={NODE_W - 16}
                  y={20}
                  textAnchor="middle"
                  className="fill-dim text-[10px]"
                >
                  ×{node.attempts}
                </text>
              </>
            ) : null}
            {p.step === start ? (
              <text x={NODE_W - 8} y={NODE_H - 8} textAnchor="end" className="fill-faint text-[9px] uppercase tracking-wide">
                {t("workflowGraph.start")}
              </text>
            ) : null}
          </g>
        );
      })}

      {/* Conditions last, on a plate cut out of the panel: an edge label is a
          legend engraved beside the run it names, and it has to survive
          crossing a node or another edge. */}
      {drawn.map((e, i) =>
        e.condition ? (
          <g key={`label-${e.from}-${e.to}-${i}`}>
            <rect
              x={e.labelX - (e.condition.length * LABEL_CHAR_W) / 2 - 4}
              y={e.labelY - LABEL_H + 4}
              width={e.condition.length * LABEL_CHAR_W + 8}
              height={LABEL_H}
              rx={3}
              className="fill-panel"
            />
            <text
              x={e.labelX}
              y={e.labelY}
              textAnchor="middle"
              className="fill-faint text-[10px] font-mono"
            >
              {/* The gap holds a fingerprint, not the whole expression — the
                  condition itself is one click away in the step's form. */}
              <title>{e.fullCondition}</title>
              {e.condition}
            </text>
          </g>
        ) : null,
      )}
    </svg>
  );
}
