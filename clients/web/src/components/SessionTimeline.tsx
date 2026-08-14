import type { Bar, BarKind, Lane, Timeline } from "../lib/timeline";
import { MAX_BAR_PX } from "../lib/timeline";
import { cn } from "../lib/cn";

/** A session's shape, drawn along one axis.
 *
 * Plain positioned divs rather than SVG: `WorkflowGraph` is SVG because it has
 * edges to route around ranks, and this has nothing to route. Divs arrive with
 * hover, focus and keyboard activation already working, and a few hundred of
 * them is not a rendering problem.
 *
 * Every lane shares one scroller so they cannot drift out of alignment, and the
 * label gutter is sticky inside it so a lane stays identifiable however far
 * right you have scrolled.
 */

const LANE_H = 34;
const BAR_H = 22;
const GUTTER_W = 148;

/** Lit by the same lamps as the rest of the console — amber for work in
 * motion, ok for something that landed, orange for a question waiting on a
 * person — so all three skins work without a fourth set of colours. */
const BAR_CLASS: Record<BarKind, string> = {
  user: "bg-raised border-rule-strong",
  assistant: "bg-lamp-ok-quiet border-lamp-ok",
  thinking: "bg-screen border-rule",
  tool: "bg-amber-quiet border-amber",
  ask: "bg-orange-quiet border-orange",
  compaction: "bg-transparent border-dashed border-rule-strong",
};

export function SessionTimeline({
  timeline,
  onSelectEntry,
  onSelectAgent,
}: {
  timeline: Timeline;
  /** A bar on the main lane: go and read that entry. */
  onSelectEntry: (entryId: string) => void;
  /** A lane: go and open that agent. */
  onSelectAgent: (agentId: string) => void;
}) {
  if (timeline.lanes.length === 0 || timeline.lanes[0].bars.length === 0) {
    return (
      <div className="flex h-full items-center justify-center px-6" data-testid="timeline-empty">
        <p className="max-w-sm text-center text-sm leading-relaxed text-dim">
          Nothing has happened in this session yet. The timeline draws itself as
          the agent works.
        </p>
      </div>
    );
  }

  const placed = timeline.lanes.filter((l) => l.placed);
  const unplaced = timeline.lanes.filter((l) => !l.placed);
  // Subagents are work inside a turn; forks are other conversations. The same
  // distinction `SubAgentCard` and `ForkMarker` already draw, carried here.
  const firstForkAt = placed.findIndex((l) => l.kind === "fork");

  return (
    <div className="h-full overflow-auto" data-testid="session-timeline">
      <div className="relative min-w-full pb-4" style={{ width: GUTTER_W + timeline.width + 48 }}>
        {/* Collapsed idle stretches, behind everything else. */}
        {timeline.gaps.map((g) => (
          <div
            key={g.x}
            data-testid="timeline-gap"
            title={`${humanGap(g.elapsedMs)} with nothing happening`}
            className="absolute top-0 bottom-0 bg-[repeating-linear-gradient(135deg,var(--rule)_0_1px,transparent_1px_5px)]"
            style={{ left: GUTTER_W + g.x, width: 20 }}
          >
            <span className="legend absolute top-1 left-1/2 origin-top-left rotate-90 whitespace-nowrap">
              {humanGap(g.elapsedMs)}
            </span>
          </div>
        ))}

        {/* One tick per turn start. Not a regular grid: the axis is monotone
            but not linear, so evenly spaced times are not a thing it has. */}
        <div className="relative h-5">
          {timeline.ticks.map((t) => (
            <span
              key={t.x}
              data-testid="timeline-tick"
              className="legend absolute top-1"
              style={{ left: GUTTER_W + t.x }}
            >
              {t.label}
            </span>
          ))}
        </div>

        {placed.map((lane, i) => (
          <div key={lane.agentId}>
            {i === firstForkAt && firstForkAt > 0 && (
              <div className="flex items-center gap-3 py-2 pr-4 pl-3">
                <span className="legend whitespace-nowrap">forked conversations</span>
                <span className="h-px flex-1 bg-[var(--rule)]" />
              </div>
            )}
            <LaneRow lane={lane} onSelectEntry={onSelectEntry} onSelectAgent={onSelectAgent} />
          </div>
        ))}

        {unplaced.length > 0 && (
          <div className="mt-3 border-t pt-2">
            {/* Shown rather than dropped: an agent nobody can find is worse
                than one drawn outside the axis and said to be. */}
            <p className="legend pb-1 pl-3">not on the timeline — nothing was recorded about when these ran</p>
            {unplaced.map((lane) => (
              <LaneRow
                key={lane.agentId}
                lane={lane}
                onSelectEntry={onSelectEntry}
                onSelectAgent={onSelectAgent}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function LaneRow({
  lane,
  onSelectEntry,
  onSelectAgent,
}: {
  lane: Lane;
  onSelectEntry: (entryId: string) => void;
  onSelectAgent: (agentId: string) => void;
}) {
  return (
    <div
      data-testid={`timeline-lane-${lane.agentId}`}
      data-kind={lane.kind}
      data-placed={lane.placed ? "true" : "false"}
      className="relative flex items-center"
      style={{ height: LANE_H }}
    >
      <div
        className="sticky left-0 z-10 shrink-0 truncate bg-panel pr-2"
        style={{ width: GUTTER_W, paddingLeft: 12 + lane.depth * 12 }}
      >
        {lane.kind === "main" ? (
          <span className="text-xs font-medium text-legend">{lane.label}</span>
        ) : (
          <button
            type="button"
            className="max-w-full truncate text-xs text-faint hover:text-legend"
            onClick={() => onSelectAgent(lane.agentId)}
            title={`Open ${lane.label} — ${lane.status}`}
          >
            {lane.label}
          </button>
        )}
      </div>

      <div className="relative flex-1" style={{ height: LANE_H }}>
        {lane.bars.map((bar) => (
          <BarView key={bar.key} bar={bar} onSelect={onSelectEntry} />
        ))}

        {lane.span && (
          <button
            type="button"
            data-testid={`timeline-span-${lane.agentId}`}
            data-status={lane.status}
            onClick={() => onSelectAgent(lane.agentId)}
            title={`${lane.label} — ${lane.status}`}
            className={cn(
              "absolute top-1/2 -translate-y-1/2 rounded-[var(--radius-chip)] border transition-[filter] hover:brightness-110",
              lane.kind === "fork" ? "border-orange bg-orange-quiet" : "border-rule-strong bg-raised",
              lane.status === "failed" && "!border-red !bg-red-quiet",
              // An open span has no known end, so it fades out rather than
              // claiming one.
              lane.span.open && "opacity-60",
            )}
            style={{ left: lane.span.x, width: lane.span.width, height: BAR_H - 6 }}
          />
        )}
      </div>
    </div>
  );
}

function BarView({ bar, onSelect }: { bar: Bar; onSelect: (entryId: string) => void }) {
  // A bar at the cap is drawn shorter than the truth. Marked, so the picture
  // does not quietly lie; the tooltip always carries the real duration.
  const capped = bar.width >= MAX_BAR_PX;
  return (
    <button
      type="button"
      data-testid={`timeline-bar-${bar.entryId}`}
      data-kind={bar.kind}
      onClick={() => onSelect(bar.entryId)}
      title={`${bar.title} · ${bar.detail}${capped ? " (drawn short)" : ""}`}
      className={cn(
        "absolute top-1/2 -translate-y-1/2 rounded-[var(--radius-chip)] border transition-[filter] hover:brightness-110",
        BAR_CLASS[bar.kind],
        bar.live && "animate-pulse",
        capped && "!border-r-4 !border-r-dashed",
      )}
      style={{ left: bar.x, width: bar.width, height: BAR_H }}
    />
  );
}

function humanGap(ms: number): string {
  const m = Math.round(ms / 60_000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h ${m % 60}m` : `${Math.round(h / 24)}d`;
}
