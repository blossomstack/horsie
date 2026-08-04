import { layoutGraph, type LayoutEdge, type LayoutNode } from "../lib/graphLayout";
import { cn } from "../lib/cn";

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

const NODE_W = 168;
const NODE_H = 56;
const GAP_X = 32;
const GAP_Y = 56;
const PAD = 16;

/** A node reads as a panel key, lit by the same lamp colours the rest of the
 * console uses: amber for work in motion, ok for a step that landed, red for a
 * fault. An unlit node is one the run has not reached. */
const STATE_CLASS: Record<NodeState, string> = {
  idle: "fill-raised stroke-rule",
  running: "fill-amber-quiet stroke-amber",
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
  const layout = layoutGraph(nodes, edges, start);
  const byStep = new Map(nodes.map((n) => [n.step, n]));
  const placed = new Map(layout.nodes.map((n) => [n.step, n]));

  // Centre each rank, so a branch reads as a fork rather than as a left edge.
  const widest = Math.max(layout.width, 1);
  const centreX = (rank: number) => {
    const count = layout.nodes.filter((n) => n.rank === rank).length;
    return ((widest - count) * (NODE_W + GAP_X)) / 2;
  };
  const position = (step: string) => {
    const p = placed.get(step);
    if (!p) return { x: 0, y: 0 };
    return {
      x: PAD + centreX(p.rank) + p.order * (NODE_W + GAP_X),
      y: PAD + p.rank * (NODE_H + GAP_Y),
    };
  };

  const width = PAD * 2 + widest * NODE_W + (widest - 1) * GAP_X;
  const height = PAD * 2 + layout.height * NODE_H + Math.max(0, layout.height - 1) * GAP_Y;

  if (layout.nodes.length === 0) {
    return (
      <p className={cn("text-sm text-faint", className)}>
        Add a step to see the graph.
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
      aria-label="Workflow graph"
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

      {layout.edges.map((e, i) => {
        const from = position(e.from);
        const to = position(e.to);
        const meta = edges.find((x) => x.from === e.from && x.to === e.to);
        const x1 = from.x + NODE_W / 2;
        const y1 = from.y + NODE_H;
        const x2 = to.x + NODE_W / 2;
        const y2 = to.y;
        // A back-edge bows out to the side so it cannot be mistaken for
        // forward progress through the graph.
        const d = e.back
          ? `M ${x1} ${from.y + NODE_H / 2} C ${x1 + NODE_W} ${from.y + NODE_H / 2}, ${
              x2 + NODE_W
            } ${to.y + NODE_H / 2}, ${x2 + NODE_W / 2 + 4} ${to.y + NODE_H / 2}`
          : `M ${x1} ${y1} C ${x1} ${y1 + GAP_Y / 2}, ${x2} ${y2 - GAP_Y / 2}, ${x2} ${y2}`;
        return (
          <g key={`${e.from}-${e.to}-${i}`}>
            <path
              d={d}
              fill="none"
              markerEnd="url(#wf-arrow)"
              className={cn(
                "stroke-[1.5]",
                meta?.taken ? "stroke-amber" : "stroke-rule-strong opacity-60",
              )}
              strokeDasharray={e.back ? "4 3" : undefined}
            />
            {meta?.condition ? (
              <text
                x={(x1 + x2) / 2 + 6}
                y={(y1 + y2) / 2}
                className="fill-faint text-[10px] font-mono"
              >
                {meta.condition.length > 24
                  ? `${meta.condition.slice(0, 23)}…`
                  : meta.condition}
              </text>
            ) : null}
          </g>
        );
      })}

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
                selected === p.step && "stroke-amber stroke-[2.5]",
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
                start
              </text>
            ) : null}
          </g>
        );
      })}
    </svg>
  );
}
