import { ChevronDown, ChevronRight, MessageSquareText } from "lucide-react";
import type { Bar, BarKind, Lane, Timeline } from "../lib/timeline";
import { i18n } from "../i18n";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";

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
/** The name column. Wide enough for a real title: at 170 a name had about
 * twenty characters between the chevron and the jump key, so most agents read
 * as a truncation with an ellipsis where the distinguishing part had been.
 * It costs the bars nothing — the pane scrolls sideways, and the sidebar is
 * pinned over it rather than sharing the width. */
const SIDEBAR_W = 240;

/** Solid, square and borderless: a bar is a block of colour, nothing else.
 *
 * The `-quiet` fills plus a read as little containers with rounded
 * corners — at the width a fast tool call earns, a 1px on each side and
 * a radius left almost no fill to see, so a lane of real work looked like a row
 * of empty chips. The strong lamp colours carry it on their own. */
const BAR_CLASS: Record<BarKind, string> = {
  user: "bg-legend",
  assistant: "bg-lamp-ok",
  thinking: "bg-rule-strong",
  tool: "bg-live",
  ask: "bg-accent",
  compaction: "bg-rule-strong",
};

/** A lane's own colour is what became of the agent, which is the one thing a
 * span can say that a bar cannot. */
const SPAN_CLASS: Record<string, string> = {
  running: "bg-live",
  provisioning: "bg-live",
  awaiting_input: "bg-accent",
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
  onOpenAgent,
  selectedAgent,
  selectedEntry,
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
  /** A bar: show what that entry was, beside the picture. */
  onSelectEntry: (entryId: string) => void;
  /** A lane's name: show what that agent is, beside the picture. */
  onSelectAgent: (agentId: string) => void;
  /** The jump key: leave the timeline for that agent's transcript. */
  onOpenAgent: (agentId: string) => void;
  /** Whichever agent the panel is showing. */
  selectedAgent?: string;
  /** Whichever entry the panel is showing. */
  selectedEntry?: string;
}) {
  const { t } = useTranslation();
  // An empty axis, rather than a first lane with no bars on it. Two things
  // break that older reading: a workflow run's root lane is the run, which has
  // no transcript of its own and so never has bars, and folding the root hides
  // every lane that does — so a run reported an empty session, and folding one
  // up replaced it with "nothing has happened yet".
  //
  // The width is the scale's, and the scale is built from everything the
  // session has done. Zero means there was nothing to lay out at all.
  if (timeline.lanes.length === 0 || timeline.width === 0) {
    return (
      <div className="flex h-full items-center justify-center px-6" data-testid="timeline-empty">
        <p className="max-w-sm text-center text-sm leading-relaxed text-dim">
{t("timeline.empty")}
        </p>
      </div>
    );
  }

  // Folding is `buildTimeline`'s, not this component's. It used to be done
  // here, by skipping lanes deeper than a folded one — and it silently did
  // nothing, because every lane hanging off the root arrived at the root's own
  // depth: nothing was ever *deeper* than the lane it was under, so nothing was
  // ever skipped. Doing it in the model also means a fold and the tree it folds
  // are decided in one place.
  const placed = timeline.lanes.filter((l) => l.placed);
  const unplaced = timeline.lanes.filter((l) => !l.placed);

  // Every lane's top, so a connector can be drawn from a lane all the way back
  // up to the one it branched from. Computed here rather than in the model
  // because a row's height is a rendering decision.
  //
  // Subagents come before sub sessions under every agent, which is the model's
  // doing — `bySibling` in `agentTree`. There used to be a labelled rule drawn
  // between the two groups as well, and it could only ever be drawn once, at
  // the first sub session in a flat list of lanes: under a tree where each
  // agent has both, it landed in the middle of one agent's children and said
  // nothing true about the rest. The grouping carries it.
  const tops = new Map<string, number>();
  let y = 0;
  placed.forEach((lane) => {
    tops.set(lane.agentId, y);
    y += LANE_H;
  });

  return (
    // The ground comes from `SessionPane`, shared with the transcript and
    // the graph.
    <div className="h-full overflow-auto" data-testid="session-timeline">
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

        {placed.map((lane) => (
          <LaneRow
            key={lane.agentId}
            lane={lane}
            rise={(tops.get(lane.agentId) ?? 0) - (tops.get(lane.anchor?.parentAgentId ?? "") ?? 0)}
            expanded={expanded.includes(lane.agentId)}
            collapsed={collapsed.includes(lane.agentId)}
            onToggleCollapse={onToggleCollapse}
            onToggleExpand={onToggleExpand}
            onSelectEntry={onSelectEntry}
            onSelectAgent={onSelectAgent}
            onOpenAgent={onOpenAgent}
            selectedAgent={selectedAgent}
            selectedEntry={selectedEntry}
          />
        ))}

        {unplaced.length > 0 && (
          <div className="mt-3 pt-2">
            {/* Shown rather than dropped: an agent nobody can find is worse
                than one drawn outside the axis and said to be. */}
            <p className="legend pb-1 pl-3">{t("timeline.unplaced")}</p>
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
                onOpenAgent={onOpenAgent}
                selectedAgent={selectedAgent}
                selectedEntry={selectedEntry}
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
  onOpenAgent,
  selectedAgent,
  selectedEntry,
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
  onOpenAgent: (agentId: string) => void;
  selectedAgent?: string;
  selectedEntry?: string;
}) {
  const sub = lane.kind !== "main";
  const isSelected = selectedAgent === lane.agentId;
  return (
    <div
      data-testid={`timeline-lane-${lane.agentId}`}
      data-kind={lane.kind}
      data-placed={lane.placed ? "true" : "false"}
      data-expanded={expanded ? "true" : undefined}
      data-selected={isSelected ? "true" : undefined}
      className={cn(
        "group relative flex items-center",
        // Which run you are looking at, when the transcript key would open one.
        // Without it the three views were three pictures of the same session
        // with no shared "you are here".
        isSelected && "bg-raised",
      )}
      style={{ height: LANE_H }}
    >
      {/* The sidebar. Sticky rather than a separate column so it cannot drift
          out of vertical step with the lanes it names. */}
      <div
        className="sticky left-0 z-20 flex h-full shrink-0 items-center gap-1 bg-panel pr-2"
        style={{ width: SIDEBAR_W, paddingLeft: 8 + lane.depth * 10 }}
      >
        {/* The chevron discloses the lanes *hanging off* this one — the
            subagents it spawned, the sub sessions taken from it. Its own work is the
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
        {/* The name shows what this agent *is*, beside the picture. It used
            to navigate to the agent's own page, which answered "what is this
            lane?" by closing the timeline that raised the question. Leaving is
            still one key away, on the right. */}
        <button
          type="button"
          data-testid={`timeline-select-${lane.agentId}`}
          aria-pressed={isSelected}
          className={cn(
            "min-w-0 flex-1 truncate text-left text-xs hover:text-legend",
            lane.kind === "main" ? "font-medium text-legend" : "text-faint",
            isSelected && "!text-legend",
          )}
          onClick={() => onSelectAgent(lane.agentId)}
        >
          {lane.label}
        </button>
        {/* Straight to that agent's transcript. Only on hover or focus: a key
            on every row at rest turns a sidebar of names into a sidebar of
            controls. */}
        {lane.kind !== "main" && (lane.kind !== "run" || lane.depth === 0) && (
          <button
            type="button"
            data-testid={`timeline-open-${lane.agentId}`}
            className="shrink-0 text-faint opacity-0 group-hover:opacity-100 focus:opacity-100 hover:text-legend"
            // A run has no transcript of its own: its page is its graph.
            title={openLabel(lane)}
            aria-label={openLabel(lane)}
            onClick={() => onOpenAgent(lane.agentId)}
          >
            {/* The transcript's own glyph, the one a graph node's key and
                both panels carry. One action, one icon. */}
            <MessageSquareText size={12} aria-hidden />
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
            className="pointer-events-none absolute top-1/2 left-[calc(100%-0.5rem)] z-30 hidden w-60 -translate-y-1/2 rounded-[var(--radius-control)] bg-panel px-2.5 py-1.5 shadow-lg group-hover:block"
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
          <BarView
            key={bar.key}
            bar={bar}
            onSelect={onSelectEntry}
            selected={selectedEntry === bar.entryId}
          />
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

function BarView({
  bar,
  onSelect,
  selected,
}: {
  bar: Bar;
  onSelect: (entryId: string) => void;
  selected: boolean;
}) {
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
      data-selected={selected ? "true" : undefined}
      aria-pressed={selected}
      onClick={() => onSelect(bar.entryId)}
      title={`${bar.title} · ${bar.detail}`}
      className={cn(
        "absolute top-1/2 -translate-y-1/2 transition-[filter] hover:brightness-125",
        BAR_CLASS[bar.kind],
        bar.live && "animate-pulse",
        // A ring rather than a colour change: the fill already means what kind
        // of work this was, and overwriting it to say "selected" would cost the
        // one thing the bar exists to show.
        selected && "ring-2 ring-legend ring-offset-1 ring-offset-[var(--panel)]",
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

/** What a lane's jump key opens, in the words of what it opens.
 *
 * Drawn only where there is somewhere to go: an agent's transcript, or — for
 * the session's own run, the lane at the top — its run view. A run an agent
 * invoked has no page of its own, and a key that went to the session instead
 * was answering a different question than the one it asked. */
function openLabel(lane: Lane): string {
  return lane.kind === "run"
    ? i18n.t("agentGraph.openRun", { label: lane.label })
    : i18n.t("agentGraph.openTranscript", { label: lane.label });
}

function humanGap(ms: number): string {
  const m = Math.round(ms / 60_000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h ${m % 60}m` : `${Math.round(h / 24)}d`;
}
