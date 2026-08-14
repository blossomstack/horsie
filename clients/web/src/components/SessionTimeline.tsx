import type { Bar, BarKind, Lane, Timeline } from "../lib/timeline";
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
/** Solid, square and borderless: a bar is a block of colour, nothing else.
 *
 * The `-quiet` fills plus a border read as little containers with rounded
 * corners — at the width a fast tool call earns, a 1px border on each side and
 * a radius left almost no fill to see, so a lane of real work looked like a row
 * of empty chips. The strong lamp colours carry it on their own. */
const BAR_CLASS: Record<BarKind, string> = {
  user: "bg-legend",
  assistant: "bg-lamp-ok",
  thinking: "bg-rule-strong",
  tool: "bg-amber",
  ask: "bg-orange",
  compaction: "bg-rule-strong",
};

/** A lane's own colour is what became of the agent, which is the one thing a
 * span can say that a bar cannot. Neutral grey on grey made a fast subagent
 * invisible at the width its duration earns it. */
const SPAN_CLASS: Record<string, string> = {
  running: "bg-amber",
  provisioning: "bg-amber",
  awaiting_input: "bg-orange",
  completed: "bg-lamp-ok",
  failed: "bg-red",
  cancelled: "bg-rule-strong",
  idle: "bg-rule-strong",
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
    // `bg-panel` on the scroller, not just on the sticky gutter: with only the
    // gutter painted, each lane's label sat in a panel-coloured rectangle
    // against the pane's own surface and read as a stray box down the left.
    <div className="h-full overflow-auto bg-panel" data-testid="session-timeline">
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
        {/* Where this lane came from. Drawn upward out of the row rather than
            as cross-row geometry: the parent is the lane above (or a lane
            above, for a nested one), and a line that leaves the top edge under
            an arrowhead says "this hangs off the timeline" without needing
            every row's height to be known in advance. */}
        {lane.anchor && (
          <span
            data-testid={`timeline-anchor-${lane.agentId}`}
            aria-hidden
            className="pointer-events-none absolute top-0 w-px bg-[var(--rule-strong)]"
            style={{ left: lane.anchor.x, height: LANE_H / 2 }}
          >
            <span
              className="absolute -top-px -left-[3px] h-0 w-0 border-r-[3px] border-b-[4px] border-l-[3px] border-r-transparent border-b-[var(--rule-strong)] border-l-transparent"
            />
          </span>
        )}
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
              "absolute top-1/2 -translate-y-1/2 transition-[filter] hover:brightness-125",
              SPAN_CLASS[lane.status] ?? "bg-raised",
              // A span is context, a bar is an event, so a lane sits at half
              // strength and never competes with the work drawn above it.
              "opacity-45",
            )}
            // An open span has no known end. It runs to the edge of the pane
            // rather than to the end of the drawn session: bounded by
            // `scale.width` a fork taken near the end of a session was a
            // thirteen-pixel stub that looked like a measured span.
            style={
              lane.span.open
                ? { left: lane.span.x, right: 8, height: BAR_H - 6 }
                : { left: lane.span.x, width: lane.span.width, height: BAR_H - 6 }
            }
          />
        )}
      </div>
    </div>
  );
}

function BarView({ bar, onSelect }: { bar: Bar; onSelect: (entryId: string) => void }) {
  // A compaction is a boundary, not a stretch of work, so it stays a hairline
  // the full height of the lane rather than a block with a width to misread.
  const boundary = bar.kind === "compaction";
  return (
    <button
      type="button"
      // Keyed on the bar, not on the entry: several bars share one message id
      // (its thinking, its text, each of its tool calls), and an id that
      // repeats is a duplicate testid Playwright's strict mode will fail on.
      data-testid={`timeline-bar-${bar.key}`}
      data-entry-id={bar.entryId}
      data-kind={bar.kind}
      onClick={() => onSelect(bar.entryId)}
      title={`${bar.title} · ${bar.detail}`}
      className={cn(
        "absolute top-1/2 -translate-y-1/2 transition-[filter] hover:brightness-125",
        BAR_CLASS[bar.kind],
        bar.live && "animate-pulse",
      )}
      style={{
        left: bar.x,
        // A hairline off the right so two square bars butt-joined at the same
        // pixel still read as two. Never below 2px, or a fast call vanishes.
        width: boundary ? 2 : Math.max(2, bar.width - 1),
        height: boundary ? BAR_H + 8 : BAR_H,
      }}
    />
  );
}

function humanGap(ms: number): string {
  const m = Math.round(ms / 60_000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h ${m % 60}m` : `${Math.round(h / 24)}d`;
}
