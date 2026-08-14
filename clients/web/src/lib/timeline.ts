/** Laying a session out along a horizontal axis.
 *
 * The axis is wall-clock order with the dead air taken out. A session is mostly
 * waiting — for a person to come back, for a long tool call — and an honest
 * linear axis spends almost all of its width on nothing. So: real elapsed time
 * between entries, up to a minute; past that, a fixed gutter labelled with what
 * it swallowed.
 *
 * The consequence, and the reason this hands back a `toX` function rather than
 * a multiplier: the drawn axis is monotone but NOT linear in time, so no caller
 * can turn a timestamp into a pixel by arithmetic. Every off-lane moment — a
 * subagent's spawn, a fork's branch point — goes through `toX`.
 */

import { MAIN_AGENT } from "../api/client";
import type { ForkView, SubAgentView } from "../api/types";
import type { RenderedMessage, TranscriptItem } from "../hooks/useSessionStream";
import { isAskCall } from "./askUser";
import { forkTree } from "./forkTree";

/** A gap longer than this is dead air, not part of the work. */
export const GAP_THRESHOLD_MS = 60_000;
/** What a gap is drawn at, at most — collapsed or kept. No stretch of nothing
 * happening is ever wider than this, so a gap can never dominate the picture. */
export const GAP_PX = 24;
/** Small enough to read as brief, big enough to still be a click target. */
export const MIN_BAR_PX = 6;
/**
 * What the longest single thing that happened is drawn at. The scale falls out
 * of this: everything else is in proportion to it.
 *
 * Scaling off the longest bar rather than off the session's total is what makes
 * a three-second session and a three-day one both readable. The first version
 * scaled the total to a fixed target and then clamped the scale, which capped a
 * short session at twenty pixels a second — a session that finished in a few
 * seconds drew as a hundred-pixel smudge in a thousand-pixel pane. It is only
 * visible in a screenshot; every unit test passed.
 */
export const TARGET_LONGEST_PX = 240;

export interface Span {
  startMs: number;
  endMs: number;
}

export interface Scale {
  /** Where a moment lands, in pixels from the left edge. Clamped at both ends. */
  toX(ms: number): number;
  width: number;
  gaps: { x: number; elapsedMs: number }[];
}

/**
 * Build the time-to-pixel map from the spans that will be drawn on the main lane.
 *
 * Spans are taken in the order given and assumed non-overlapping, which is what
 * a transcript produces: an assistant message and the tools it issued are
 * consecutive, and parallel tool calls share a start but are laid out in issue
 * order.
 */
export function buildScale(spans: Span[]): Scale {
  if (spans.length === 0) {
    return { toX: () => 0, width: 0, gaps: [] };
  }

  // The longest thing that happened sets the scale, and everything else is
  // drawn in proportion to it. Zero when every span is instantaneous — a
  // session of nothing but user messages — in which case every bar is the
  // minimum and there is nothing to be in proportion to.
  let longestMs = 0;
  for (const s of spans) longestMs = Math.max(longestMs, s.endMs - s.startMs);
  const scale = longestMs > 0 ? TARGET_LONGEST_PX / longestMs : 0;

  // Breakpoints: (ms, px) pairs, both increasing. Between two consecutive
  // points the map is linear, which is what makes `toX` one interpolation
  // rather than a special case per kind of interval.
  const ms: number[] = [];
  const px: number[] = [];
  const gaps: { x: number; elapsedMs: number }[] = [];
  let x = 0;

  // A zero-duration span — a user message — would otherwise put two points at
  // the same instant with different pixels, and the interpolation below would
  // divide by zero. The later pixel wins, so the bar still has its width.
  const push = (atMs: number, atPx: number) => {
    if (ms.length > 0 && atMs === ms[ms.length - 1]) {
      px[px.length - 1] = atPx;
      return;
    }
    ms.push(atMs);
    px.push(atPx);
  };

  push(spans[0].startMs, 0);
  for (let i = 0; i < spans.length; i++) {
    if (i > 0) {
      const gap = spans[i].startMs - spans[i - 1].endMs;
      if (gap > GAP_THRESHOLD_MS) {
        gaps.push({ x, elapsedMs: gap });
        x += GAP_PX;
      } else if (gap > 0) {
        // Capped at the same width a collapsed gap gets, so waiting never
        // outdraws working: at a short session's scale a proportional
        // fifty-second pause would be wider than every bar put together.
        x += Math.min(gap * scale, GAP_PX);
      }
      push(spans[i].startMs, x);
    }
    const duration = Math.max(0, spans[i].endMs - spans[i].startMs);
    // A zero-duration span — a user message — still gets a minimum so it stays
    // clickable. Nothing caps the top: the longest bar *is* the scale, so no
    // bar can run away, and every width is honest.
    x += Math.max(duration * scale, MIN_BAR_PX);
    push(spans[i].endMs, x);
  }

  const width = x;
  const toX = (at: number): number => {
    if (at <= ms[0]) return 0;
    if (at >= ms[ms.length - 1]) return width;
    // A linear scan: a session has hundreds of breakpoints, not millions, and
    // this runs once per off-lane agent rather than once per frame.
    for (let i = 1; i < ms.length; i++) {
      if (at <= ms[i]) {
        const spanMs = ms[i] - ms[i - 1];
        const t = spanMs === 0 ? 0 : (at - ms[i - 1]) / spanMs;
        return px[i - 1] + t * (px[i] - px[i - 1]);
      }
    }
    return width;
  };

  return { toX, width, gaps };
}

// ---------------------------------------------------------------------------
// Lanes

export type BarKind = "user" | "assistant" | "thinking" | "tool" | "ask" | "compaction";
export type LaneKind = "main" | "subagent" | "fork";

export interface Bar {
  key: string;
  kind: BarKind;
  x: number;
  width: number;
  /** What a click scrolls the transcript to: a message id, or a compaction seq. */
  entryId: string;
  title: string;
  detail: string;
  /** Still running: drawn against `nowMs`, so the width is provisional. */
  live?: boolean;
}

export interface Lane {
  agentId: string;
  kind: LaneKind;
  label: string;
  status: string;
  depth: number;
  /** The main lane always has bars. Another lane has them once it has been
   * expanded and its own history fetched; until then it is a span. */
  bars: Bar[];
  span?: { x: number; width: number; open: boolean };
  anchor?: { x: number; parentAgentId: string };
  /** False when nothing recorded about this agent could place it on the axis. */
  placed: boolean;
  /** Whether any lane hangs off this one — what decides if it gets a
   * disclosure control at all. */
  hasChildren: boolean;
  /** Status, how long it took and when it started, for the hover card. The
   * sidebar can only ever show a truncated name. */
  detail: string;
}

export interface Timeline {
  lanes: Lane[];
  gaps: { x: number; elapsedMs: number }[];
  ticks: { x: number; label: string }[];
  width: number;
}

/** One thing drawn on the main lane, before it has a position. */
interface Entry {
  kind: BarKind;
  entryId: string;
  startMs: number;
  endMs: number;
  title: string;
  live: boolean;
  /** A user message starts a turn, and a turn start is where a tick goes. */
  turnStart: boolean;
}

function humanMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${Math.floor(ms / 3_600_000)}h ${Math.round((ms % 3_600_000) / 60_000)}m`;
}

/** What a lane says about itself beyond its name: what became of it, how long
 * it took, and when it started. The sidebar can only show a truncated name, so
 * this is what the hover card carries. */
function describe(status: string, startMs: number, endMs: number): string {
  const parts = [status.replace(/_/g, " ")];
  if (startMs > 0 && endMs > startMs) parts.push(humanMs(endMs - startMs));
  if (startMs > 0) parts.push(`started ${clockTime(startMs)}`);
  return parts.join(" · ");
}

/** 24-hour, so a label is five characters wide however long the session ran. */
function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/** How close two turn-start labels may sit before the later one is dropped. */
const TICK_MIN_GAP_PX = 56;

/** Statuses that mean the agent has not stopped, so its lane runs to the edge
 * rather than ending at whatever it last did. */
const LIVE_STATUS = new Set(["running", "provisioning", "awaiting_input"]);

/**
 * Flatten one message into the things the timeline draws.
 *
 * The time rules are `transcriptSegments.ts`'s rather than a second set: an
 * assistant message spans the provider call that produced it, and its tool
 * calls were issued at the end of that call and each ended when its result
 * landed. A tool result carries no timestamps of its own — `ToolResultPart` has
 * none — so that interval is the only way its duration can be known.
 *
 * A subagent's report is deliberately not drawn. It arrives inside a user
 * message, and the subagent already has a lane.
 */
function entriesOf(m: RenderedMessage, nowMs: number): Entry[] {
  const out: Entry[] = [];
  const ended = m.createdAtMs ?? 0;
  const began = m.startedAtMs ?? ended;

  if (m.role === "User") {
    // Only a message a person actually sent. One carrying nothing but a
    // subagent's report is machinery, not a turn.
    if (m.text) {
      out.push({
        kind: "user",
        entryId: m.id,
        startMs: ended,
        endMs: ended,
        title: m.text.slice(0, 80),
        live: false,
        turnStart: true,
      });
    }
    return out;
  }

  if (m.thinking.length > 0) {
    out.push({
      kind: "thinking",
      entryId: m.id,
      startMs: began,
      endMs: ended,
      title: `Thinking · ${m.thinking.length} block${m.thinking.length > 1 ? "s" : ""}`,
      live: false,
      turnStart: false,
    });
  }
  if (m.text) {
    // After the thinking when there was any: they share the one provider call,
    // and two bars claiming the same interval would double its width.
    out.push({
      kind: "assistant",
      entryId: m.id,
      startMs: m.thinking.length > 0 ? ended : began,
      endMs: ended,
      title: m.text.slice(0, 80),
      live: false,
      turnStart: false,
    });
  }
  // Every tool call in one message was issued at the same instant, so laying
  // them out at their true starts stacked them on top of each other — two
  // parallel `bash` calls drew as one bar with the shorter hidden under the
  // longer. Laid end to end in finish order instead: each bar keeps its own
  // true duration, and the run of them reads as the work that message set off.
  // The cost is that the picture no longer says they overlapped in time.
  let issuedAt = ended;
  for (const call of [...m.toolCalls].sort(
    (a, b) => (a.endedAtMs ?? nowMs) - (b.endedAtMs ?? nowMs),
  )) {
    const took = Math.max(0, (call.endedAtMs ?? nowMs) - ended);
    out.push({
      kind: isAskCall(call.name, call.input) ? "ask" : "tool",
      entryId: m.id,
      startMs: issuedAt,
      endMs: issuedAt + took,
      title: call.name,
      live: call.running,
      turnStart: false,
    });
    issuedAt += took;
  }
  return out;
}

/**
 * Lay a session out: the main agent's entries as bars, every other agent as a
 * span hanging below the lane it came from.
 *
 * `nowMs` is passed rather than read so this stays pure, and so one layout pass
 * measures every still-running bar against the same instant.
 */
export function buildTimeline(
  items: TranscriptItem[],
  agents: SubAgentView[],
  forks: ForkView[],
  nowMs: number,
  /** Histories already fetched for expanded lanes, keyed by agent id. Their
   * bars are laid out on the *session's* scale, not one of their own, so a
   * subagent's work lines up under the turn that spawned it. */
  expanded: Record<string, TranscriptItem[]> = {},
): Timeline {
  const entries: Entry[] = [];
  for (const item of items) {
    if (item.kind === "message") {
      entries.push(...entriesOf(item.value, nowMs));
    } else if (item.kind === "compaction") {
      entries.push({
        kind: "compaction",
        // The boundary's seq: the transcript already anchors dividers by it,
        // so seeking one needs nothing new.
        entryId: String(item.value.seq),
        startMs: item.value.atMs,
        endMs: item.value.atMs,
        title: "Conversation compacted",
        live: false,
        turnStart: false,
      });
    }
    // A `fork` item is not drawn here: the fork has a lane of its own and the
    // branch anchor says where it came from. A `notice` is a hook record, which
    // is not work the session spent time on.
  }
  entries.sort((a, b) => a.startMs - b.startMs);

  const scale = buildScale(entries.map((e) => ({ startMs: e.startMs, endMs: e.endMs })));

  const bars: Bar[] = entries.map((e, i) => {
    const x = scale.toX(e.startMs);
    return {
      key: `${e.entryId}:${e.kind}:${i}`,
      kind: e.kind,
      x,
      width: Math.max(MIN_BAR_PX, scale.toX(e.endMs) - x),
      entryId: e.entryId,
      title: e.title,
      detail: e.endMs > e.startMs ? humanMs(e.endMs - e.startMs) : clockTime(e.startMs),
      live: e.live || undefined,
    };
  });

  // One per turn start, minus two kinds of noise: a label that would print on
  // top of the one before it (four turns a second apart rendered four labels at
  // the same pixel, reading as one line of garbled digits), and a label that
  // repeats the one before it — several turns inside the same minute produced
  // `18:27 18:27 18:27`, three marks that say nothing.
  /** Bars for an expanded lane, on the session's own scale.
   *
   * `fromMs` is where the agent began, and everything before it is dropped: a
   * fork's log *starts as a copy of its parent's*, carrying the parent's
   * original timestamps, so drawn unfiltered a fork claimed to have been
   * working through turns that happened before it existed. A subagent's log is
   * its own, so for one the filter is a no-op — but the rule is the same one:
   * a lane shows what that agent did. */
  const barsFor = (agentId: string, fromMs: number): Bar[] => {
    const own = expanded[agentId];
    if (!own) return [];
    const es: Entry[] = [];
    for (const item of own) {
      if (item.kind === "message") es.push(...entriesOf(item.value, nowMs));
    }
    return es
      .filter((e) => e.startMs >= fromMs)
      .sort((a, b) => a.startMs - b.startMs)
      .map((e, i) => {
      const x = scale.toX(e.startMs);
      return {
        key: `${agentId}:${e.entryId}:${e.kind}:${i}`,
        kind: e.kind,
        x,
        width: Math.max(MIN_BAR_PX, scale.toX(e.endMs) - x),
        entryId: e.entryId,
        title: e.title,
        detail: e.endMs > e.startMs ? humanMs(e.endMs - e.startMs) : clockTime(e.startMs),
        live: e.live || undefined,
      };
    });
  };

  const ticks: { x: number; label: string }[] = [];
  for (const e of entries) {
    if (!e.turnStart) continue;
    const x = scale.toX(e.startMs);
    const label = clockTime(e.startMs);
    const last = ticks[ticks.length - 1];
    if (last && (x - last.x < TICK_MIN_GAP_PX || label === last.label)) continue;
    ticks.push({ x, label });
  }

  // The main agent is the one nothing spawned. Falling back to the first entry,
  // and then to the well-known id, so a roster that has not arrived yet still
  // produces a lane to draw the transcript on.
  const main = agents.find((a) => !a.parent && a.depth === 0) ?? agents[0];
  const mainId = main?.id ?? MAIN_AGENT;
  const lanes: Lane[] = [
    {
      agentId: mainId,
      kind: "main",
      label: "main agent",
      status: main?.status ?? "idle",
      depth: 0,
      bars,
      placed: true,
      hasChildren: agents.length > 1 || forks.length > 0,
      detail: main?.status ?? "idle",
    },
  ];

  /** A span, or nothing when there is no stamp to place the agent by.
   *
   * `open` is decided by the status, not by a missing end stamp: a fork always
   * has a last-activity time, so "it has no end" and "it is still going" are
   * two different questions and only the status answers the second. */
  const spanOf = (startMs: number, endMs: number, status: string) => {
    if (startMs <= 0) return undefined;
    const x = scale.toX(startMs);
    const open = LIVE_STATUS.has(status) || endMs <= 0;
    return { x, width: Math.max(MIN_BAR_PX, (open ? scale.width : scale.toX(endMs)) - x), open };
  };

  // Render order is a walk of the tree, not the roster's own order: the roster
  // is keyed by uuid, so a subagent's child could sort above it and the two
  // read as siblings. The same two rules `forkTree` learned — an orphan roots
  // at the top level, anything the walk cannot reach is appended flat — because
  // this reads the same journal-derived data.
  const held = new Set(agents.map((a) => a.id));
  const kids = new Map<string, SubAgentView[]>();
  for (const a of agents) {
    if (a.id === mainId) continue;
    const key = a.parent && held.has(a.parent) ? a.parent : "";
    kids.set(key, [...(kids.get(key) ?? []), a]);
  }
  for (const level of kids.values()) {
    level.sort((x, y) => x.spawnedAtMs - y.spawnedAtMs || x.id.localeCompare(y.id));
  }
  const seen = new Set<string>();
  const walk = (parent: string, depth: number) => {
    for (const a of kids.get(parent) ?? []) {
      if (seen.has(a.id)) continue;
      seen.add(a.id);
      const span = spanOf(a.spawnedAtMs, a.endedAtMs, a.status);
      lanes.push({
        agentId: a.id,
        kind: "subagent",
        label: a.label ?? a.agentType ?? "subagent",
        status: a.status,
        bars: barsFor(a.id, a.spawnedAtMs),
        depth,
        span,
        anchor: span ? { x: span.x, parentAgentId: parent || mainId } : undefined,
        placed: span !== undefined,
        hasChildren: (kids.get(a.id) ?? []).length > 0,
        detail: describe(a.status, a.spawnedAtMs, a.endedAtMs),
      });
      walk(a.id, depth + 1);
    }
  };
  walk("", 0);
  for (const a of agents) {
    if (a.id === mainId || seen.has(a.id)) continue;
    const span = spanOf(a.spawnedAtMs, a.endedAtMs, a.status);
    lanes.push({
      agentId: a.id,
      kind: "subagent",
      label: a.label ?? a.agentType ?? "subagent",
      status: a.status,
      bars: barsFor(a.id, a.spawnedAtMs),
      depth: 0,
      span,
      anchor: span ? { x: span.x, parentAgentId: mainId } : undefined,
      placed: span !== undefined,
      hasChildren: false,
      detail: describe(a.status, a.spawnedAtMs, a.endedAtMs),
    });
  }

  // `forkTree` already turns a flat, parent-linked list into render order with
  // a depth per row, roots an orphan at the top level and refuses to drop a
  // cycle. All of that is the same problem here.
  for (const placed of forkTree(forks)) {
    const f = placed.fork;
    // A fork is drawn exactly like a subagent: from when it branched to when it
    // last did anything. It has no *end* — nothing closes a conversation — but
    // "still running, forever" was a worse lie than "this is how far it got",
    // and it made every fork a bar running off the edge of the pane.
    const span = spanOf(f.createdAtMs, f.lastActivityMs, f.status);
    lanes.push({
      agentId: f.id,
      kind: "fork",
      label: f.title ?? "untitled fork",
      status: f.status,
      bars: barsFor(f.id, f.createdAtMs),
      depth: placed.depth,
      span,
      anchor: span
        ? { x: span.x, parentAgentId: placed.depth > 0 && f.parent ? f.parent : mainId }
        : undefined,
      placed: span !== undefined,
      hasChildren: forks.some((o) => o.parent === f.id),
      detail: describe(f.status, f.createdAtMs, f.lastActivityMs),
    });
  }

  return { lanes, gaps: scale.gaps, ticks, width: scale.width };
}
