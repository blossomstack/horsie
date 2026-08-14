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
/** What a collapsed gap is drawn at, however long it really was. */
export const GAP_PX = 20;
/** Small enough to read as brief, big enough to still be a click target. */
export const MIN_BAR_PX = 6;
/** One forty-minute tool call must not push the rest of the session off screen. */
export const MAX_BAR_PX = 320;
/** Roughly three pane-widths of drawn session. */
export const TARGET_PX = 2400;
export const MIN_SCALE = 0.0005;
export const MAX_SCALE = 0.02;

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

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

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

  // Scaled on *active* time — elapsed minus everything that will collapse —
  // rather than on total elapsed, which would squeeze a session with one
  // overnight gap in it down to a smudge.
  let activeMs = 0;
  for (let i = 0; i < spans.length; i++) {
    activeMs += Math.max(0, spans[i].endMs - spans[i].startMs);
    if (i > 0) {
      const gap = spans[i].startMs - spans[i - 1].endMs;
      if (gap > 0 && gap <= GAP_THRESHOLD_MS) activeMs += gap;
    }
  }
  const scale = activeMs > 0 ? clamp(TARGET_PX / activeMs, MIN_SCALE, MAX_SCALE) : MIN_SCALE;

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
        x += gap * scale;
      }
      push(spans[i].startMs, x);
    }
    const duration = Math.max(0, spans[i].endMs - spans[i].startMs);
    // Clamped, which is the one place the drawing stops being literally true.
    // A bar at the cap is marked in the UI and its tooltip carries the real
    // number; a zero-duration span still gets MIN_BAR_PX so it stays clickable.
    x += clamp(duration * scale, MIN_BAR_PX, MAX_BAR_PX);
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
  /** Only the main lane has bars; the rest are spans you click through to. */
  bars: Bar[];
  span?: { x: number; width: number; open: boolean };
  anchor?: { x: number; parentAgentId: string };
  /** False when nothing recorded about this agent could place it on the axis. */
  placed: boolean;
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

function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

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
  for (const call of m.toolCalls) {
    out.push({
      kind: isAskCall(call.name, call.input) ? "ask" : "tool",
      entryId: m.id,
      startMs: ended,
      endMs: call.endedAtMs ?? nowMs,
      title: call.name,
      live: call.running,
      turnStart: false,
    });
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

  const ticks = entries
    .filter((e) => e.turnStart)
    .map((e) => ({ x: scale.toX(e.startMs), label: clockTime(e.startMs) }));

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
    },
  ];

  /** A span, or nothing when there is no stamp to place the agent by. */
  const spanOf = (startMs: number, endMs: number) => {
    if (startMs <= 0) return undefined;
    const x = scale.toX(startMs);
    const open = endMs <= 0;
    return { x, width: Math.max(MIN_BAR_PX, (open ? scale.width : scale.toX(endMs)) - x), open };
  };

  const held = new Set(agents.map((a) => a.id));
  for (const a of agents) {
    if (a.id === mainId) continue;
    const span = spanOf(a.spawnedAtMs, a.endedAtMs);
    // A parent nobody holds is the same as no parent: deleting one, or never
    // having been told about it, must not hide the child. `forkTree` learned
    // this on the same journal-derived data.
    const rooted = a.parent !== undefined && held.has(a.parent);
    lanes.push({
      agentId: a.id,
      kind: "subagent",
      label: a.label ?? a.agentType ?? "subagent",
      status: a.status,
      depth: rooted ? a.depth : 0,
      bars: [],
      span,
      anchor: span ? { x: span.x, parentAgentId: rooted ? (a.parent as string) : mainId } : undefined,
      placed: span !== undefined,
    });
  }

  // `forkTree` already turns a flat, parent-linked list into render order with
  // a depth per row, roots an orphan at the top level and refuses to drop a
  // cycle. All of that is the same problem here.
  for (const placed of forkTree(forks)) {
    const f = placed.fork;
    const span = spanOf(f.createdAtMs, 0);
    lanes.push({
      agentId: f.id,
      kind: "fork",
      label: f.title ?? "untitled fork",
      status: f.status,
      depth: placed.depth,
      bars: [],
      span,
      anchor: span
        ? { x: span.x, parentAgentId: placed.depth > 0 && f.parent ? f.parent : mainId }
        : undefined,
      placed: span !== undefined,
    });
  }

  return { lanes, gaps: scale.gaps, ticks, width: scale.width };
}
