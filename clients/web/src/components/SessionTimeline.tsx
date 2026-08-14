import { ChevronDown, ChevronRight } from "lucide-react";
import type { Bar, BarKind, Lane, Timeline } from "../lib/timeline";
import { cn } from "../lib/cn";

/** A session's shape, drawn along one axis.
 *
 * Plain positioned divs rather than SVG: `WorkflowGraph` is SVG because it has
 * edges to route around ranks, and this has nothing to route. Divs arrive with
 * hover, focus and keyboard activation already working, and a few hundred of
 * them is not a rendering problem.
 *
 * One scroller for everything, moving on both axes, with the agent names held
 * against the left edge as a sidebar. Both are needed for the same reason: a
 * session with a dozen agents runs off the bottom as readily as a long one runs
 * off the right, and a lane whose name has scrolled away is unidentifiable.
 */

const LANE_H = 30;
const BAR_H = 20;
const SIDEBAR_W = 170;
/** The "forked conversations" rule, which is a row of its own in the stack. */
const DIVIDER_H = 26;

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
 * span can say that a bar cannot. */
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
  expanded,
  collapsed,
  onToggleCollapse,
  onToggleExpand,
  onSelectEntry,
  onSelectAgent,
}: {
  timeline: Timeline;
  /** Agents whose own history is being shown on their lane. */
  expanded: string[];
  /** Agents whose children are hidden. */
  collapsed: string[];
  /** Show or hide the lanes hanging off one agent. */
  onToggleCollapse: (agentId: string) => void;
  /** Show or hide one agent's own bars. */
  onToggleExpand: (agentId: string) => void;
  /** A bar: go and read that entry. */
  onSelectEntry: (entryId: string) => void;
  /** A lane's name: go and open that agent. */
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

  // A collapsed lane hides everything hanging off it. Walked by depth rather
  // than by parent id: lanes already arrive in tree order, so anything deeper
  // than a collapsed lane, until the depth comes back up, is its descendants.
  const visible: Lane[] = [];
  let hiddenBelow: number | null = null;
  for (const lane of timeline.lanes) {
    if (hiddenBelow !== null && lane.depth > hiddenBelow) continue;
    hiddenBelow = null;
    visible.push(lane);
    if (collapsed.includes(lane.agentId)) hiddenBelow = lane.depth;
  }

  const placed = visible.filter((l) => l.placed);
  const unplaced = visible.filter((l) => !l.placed);
  // Subagents are work inside a turn; forks are other conversations. The same
  // distinction `SubAgentCard` and `ForkMarker` already draw, carried here.
  const firstForkAt = placed.findIndex((l) => l.kind === "fork");

  // Every lane's top, so a connector can be drawn from a lane all the way back
  // up to the one it branched from. Computed here rather than in the model
  // because it depends on the divider, which is a rendering decision.
  const tops = new Map<string, number>();
  let y = 0;
  placed.forEach((lane, i) => {
    if (i === firstForkAt && firstForkAt > 0) y += DIVIDER_H;
    tops.set(lane.agentId, y);
    y += LANE_H;
  });

  return (
    // `bg-chassis`, which is what the body is painted and therefore what the
    // transcript pane shows through. Painted `panel` this pane was a different
    // colour from the transcript it replaces and from the composer beneath it.
    <div className="h-full overflow-auto bg-chassis" data-testid="session-timeline">
      <div className="relative w-max min-w-full pb-6">
        {/* Collapsed idle stretches, behind everything else. */}
        {timeline.gaps.map((g) => (
          <div
            key={g.x}
            data-testid="timeline-gap"
            title={`${humanGap(g.elapsedMs)} with nothing happening`}
            className="absolute top-0 bottom-0 bg-[repeating-linear-gradient(135deg,var(--rule)_0_1px,transparent_1px_5px)]"
            style={{ left: SIDEBAR_W + g.x, width: 20 }}
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
              style={{ left: SIDEBAR_W + t.x }}
            >
              {t.label}
            </span>
          ))}
        </div>

        {placed.map((lane, i) => (
          <div key={lane.agentId}>
            {i === firstForkAt && firstForkAt > 0 && (
              <div
                className="flex items-center gap-3 pr-6 pl-3"
                style={{ height: DIVIDER_H }}
              >
                <span className="legend whitespace-nowrap">forked conversations</span>
                <span className="h-px flex-1 bg-[var(--rule)]" />
              </div>
            )}
            <LaneRow
              lane={lane}
              rise={(tops.get(lane.agentId) ?? 0) - (tops.get(lane.anchor?.parentAgentId ?? "") ?? 0)}
              expanded={expanded.includes(lane.agentId)}
              collapsed={collapsed.includes(lane.agentId)}
              onToggleCollapse={onToggleCollapse}
              onToggleExpand={onToggleExpand}
              onSelectEntry={onSelectEntry}
              onSelectAgent={onSelectAgent}
            />
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
                rise={0}
                expanded={false}
                collapsed={false}
                onToggleCollapse={onToggleCollapse}
                onToggleExpand={onToggleExpand}
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
  rise,
  expanded,
  collapsed,
  onToggleCollapse,
  onToggleExpand,
  onSelectEntry,
  onSelectAgent,
}: {
  lane: Lane;
  /** Pixels from this lane up to the lane it branched from. */
  rise: number;
  expanded: boolean;
  collapsed: boolean;
  onToggleCollapse: (agentId: string) => void;
  onToggleExpand: (agentId: string) => void;
  onSelectEntry: (entryId: string) => void;
  onSelectAgent: (agentId: string) => void;
}) {
  const sub = lane.kind !== "main";
  return (
    <div
      data-testid={`timeline-lane-${lane.agentId}`}
      data-kind={lane.kind}
      data-placed={lane.placed ? "true" : "false"}
      data-expanded={expanded ? "true" : undefined}
      className="group relative flex items-center"
      style={{ height: LANE_H }}
    >
      {/* The sidebar. Sticky rather than a separate column so it cannot drift
          out of vertical step with the lanes it names. */}
      <div
        className="sticky left-0 z-20 flex h-full shrink-0 items-center gap-1 border-r bg-chassis pr-2"
        style={{ width: SIDEBAR_W, paddingLeft: 8 + lane.depth * 10 }}
      >
        {/* The chevron discloses the lanes *hanging off* this one — the
            subagents it spawned, the forks taken from it. Its own work is the
            span out on the timeline, which is where you already are looking. */}
        {lane.hasChildren ? (
          <button
            type="button"
            data-testid={`timeline-collapse-${lane.agentId}`}
            className="shrink-0 text-faint hover:text-legend"
            aria-expanded={!collapsed}
            title={collapsed ? "Show what this agent spawned" : "Hide what this agent spawned"}
            onClick={() => onToggleCollapse(lane.agentId)}
          >
            {collapsed ? <ChevronRight size={12} aria-hidden /> : <ChevronDown size={12} aria-hidden />}
          </button>
        ) : (
          <span className="w-3 shrink-0" aria-hidden />
        )}
        {lane.kind === "main" ? (
          <span className="truncate text-xs font-medium text-legend">{lane.label}</span>
        ) : (
          <button
            type="button"
            data-testid={`timeline-open-${lane.agentId}`}
            className="min-w-0 truncate text-left text-xs text-faint hover:text-legend"
            onClick={() => onSelectAgent(lane.agentId)}
          >
            {lane.label}
          </button>
        )}

        {/* The sidebar is a fixed width, so a long name is always going to be
            cut. The card is where the whole of it lives, with what the row
            cannot fit beside it. Positioned over the lanes rather than inside
            the sidebar, which is only wide enough to have caused the problem. */}
        {sub && (
          <div
            data-testid={`timeline-card-${lane.agentId}`}
            role="tooltip"
            className="pointer-events-none absolute top-1/2 left-[calc(100%-0.5rem)] z-30 hidden w-60 -translate-y-1/2 rounded-[var(--radius-control)] border bg-panel px-2.5 py-1.5 shadow-lg group-hover:block"
          >
            <p className="text-xs leading-snug break-words text-legend">{lane.label}</p>
            {/* Not `legend`: that class upper-cases and letter-spaces, which
                turned a readable "completed · 3.4s · started 09:13" into a
                shouted fragment. */}
            <p className="mt-0.5 text-[0.6875rem] leading-snug text-dim">{lane.detail}</p>
          </div>
        )}
      </div>

      <div className="relative h-full flex-1">
        {/* Where this lane came from, and where it stopped: a dashed line at
            each end running back up to the lane it branched off. Two lines
            rather than one arrow because the question a reader has is "which
            part of the parent's work was this", and one mark can only answer
            half of it. */}
        {lane.anchor && lane.span && rise > 0 && (
          <>
            <Drop x={lane.span.x} rise={rise} testid={`timeline-anchor-${lane.agentId}`} head />
            {!lane.span.open && (
              <Drop
                x={lane.span.x + lane.span.width}
                rise={rise}
                testid={`timeline-anchor-end-${lane.agentId}`}
              />
            )}
          </>
        )}

        {lane.bars.map((bar) => (
          <BarView key={bar.key} bar={bar} onSelect={onSelectEntry} />
        ))}

        {/* Once a lane is showing its own work, the span behind it would be a
            second bar saying the same thing, so it steps back to a hairline. */}
        {lane.span && (
          <button
            type="button"
            data-testid={`timeline-span-${lane.agentId}`}
            data-status={lane.status}
            onClick={() => onToggleExpand(lane.agentId)}
            title={`${lane.label} — ${lane.status}`}
            className={cn(
              "absolute top-1/2 -translate-y-1/2 transition-[filter] hover:brightness-125",
              SPAN_CLASS[lane.status] ?? "bg-rule-strong",
              expanded ? "opacity-30" : "opacity-45",
            )}
            style={{
              left: lane.span.x,
              width: lane.span.width,
              height: expanded ? 2 : BAR_H - 6,
            }}
          />
        )}
      </div>
    </div>
  );
}

/** A dashed line from a lane up to the one it branched from. */
function Drop({
  x,
  rise,
  testid,
  head,
}: {
  x: number;
  rise: number;
  testid: string;
  head?: boolean;
}) {
  return (
    <span
      data-testid={testid}
      aria-hidden
      // Only while the row is under the pointer. Drawn for every lane at once
      // they were a thicket of dashes across the whole pane, and "which part of
      // the parent's work was this" is a question you ask of one agent at a
      // time. `opacity` rather than mounting on hover so there is nothing to
      // lay out, and no flicker crossing the row.
      className="pointer-events-none absolute border-l border-dashed border-[var(--rule-strong)] opacity-0 transition-opacity group-hover:opacity-100"
      style={{ left: x, height: rise, bottom: "50%" }}
    >
      {head && (
        <span className="absolute -top-px -left-[3px] h-0 w-0 border-r-[3px] border-b-[4px] border-l-[3px] border-r-transparent border-b-[var(--rule-strong)] border-l-transparent" />
      )}
    </span>
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
