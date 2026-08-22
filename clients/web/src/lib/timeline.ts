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
 * subagent's spawn, a sub session's branch point — goes through `toX`.
 */

import { MAIN_AGENT } from "../api/client";
import type { SubSessionView, SubAgentView } from "../api/types";
import type { RenderedMessage, TranscriptItem } from "../hooks/useSessionStream";
import { isAskCall } from "./askUser";
import { type AgentKind, hostedTree } from "./agentTree";

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
export type LaneKind = AgentKind;

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

/** When a subagent or step reached its result; zero while it is still going. */
function endOfAgent(agents: SubAgentView[], id: string): number {
  return agents.find((a) => a.id === id)?.endedAtMs ?? 0;
}

/** When a sub session last did anything. Not an end — nothing closes a
 * session — but it is how far along it got, which is what a bar can draw. */
function endOfSubSession(subSessions: SubSessionView[], id: string): number {
  return subSessions.find((f) => f.id === id)?.lastActivityMs ?? 0;
}

/** What the root lane is called when nothing has named it yet. */
function sessionTitleFallback(kind: LaneKind): string {
  return kind === "main" ? "this session" : "this agent";
}

/** 24-hour, so a label is five characters wide however long the session ran. */
function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/**
 * How close two turn-start labels may sit before the later one is dropped.
 *
 * Wide, and deliberately wider than a label: at the old spacing a session of
 * short turns drew a solid band of overlapping clock times along the axis,
 * which read as a rendering fault rather than as timestamps. A tick is an
 * orientation aid — it is better to have four of them than forty.
 */
const TICK_MIN_GAP_PX = 120;

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
      kind: isAskCall(call.name) ? "ask" : "tool",
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
  subSessions: SubSessionView[],
  nowMs: number,
  /** Histories already fetched for expanded lanes, keyed by agent id. Their
   * bars are laid out on the *session's* scale, not one of their own, so a
   * subagent's work lines up under the turn that spawned it. */
  expanded: Record<string, TranscriptItem[]> = {},
  /** Which agent's work this is a timeline of. Absent means the main agent —
   * the session's own page. */
  rootAgentId?: string,
  /** Members whose children are folded away. Applied here rather than in the
   * renderer so a fold actually removes the lanes: the renderer could only
   * skip rows it was handed, and it was handed a flat list in which a
   * top-level subagent sat at the root's own depth. */
  collapsed: readonly string[] = [],
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
        title: "Session compacted",
        live: false,
        turnStart: false,
      });
    }
    // A `sub session` item is not drawn here: the sub session has a lane of its own and the
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
   * sub session's log *starts as a copy of its parent's*, carrying the parent's
   * original timestamps, so drawn unfiltered a sub session claimed to have been
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

  // What this timeline is *of*. A page scoped to one agent draws that agent's
  // work: the transcript above it is that agent's, and a lane labelled "main
  // agent" over a subagent's bars was the picture disagreeing with the prose
  // beside it. Falling back to the main agent — the one nothing spawned — and
  // then to the well-known id, so a roster that has not arrived yet still
  // produces a lane to draw the transcript on.
  const main = agents.find((a) => !a.parent && a.depth === 0) ?? agents[0];
  const rootId = rootAgentId ?? main?.id ?? MAIN_AGENT;
  const rootAgent = agents.find((a) => a.id === rootId);
  const rootSubSession = subSessions.find((f) => f.id === rootId);
  const rootKind: LaneKind = rootSubSession
    ? "sub_session"
    : rootAgent && rootAgent.id !== main?.id
      ? rootAgent.kind === "step"
        ? "step"
        : "subagent"
      : "main";
  // Orphans belong to the session, not to one agent inside it: scoped to a
  // subagent, the tree is that subagent's own subtree.
  const members = hostedTree(agents, subSessions, rootId, collapsed, rootKind === "main");

  const lanes: Lane[] = [
    {
      agentId: rootId,
      kind: rootKind,
      label:
        rootSubSession?.title ??
        rootAgent?.title ??
        rootAgent?.agentType ??
        sessionTitleFallback(rootKind),
      status: rootSubSession?.status ?? rootAgent?.status ?? "idle",
      depth: 0,
      bars,
      placed: true,
      hasChildren: members.length > 0,
      detail: rootSubSession?.status ?? rootAgent?.status ?? "idle",
    },
  ];

  /** A span, or nothing when there is no stamp to place the member by.
   *
   * `open` is decided by the status, not by a missing end stamp: a sub session
   * always has a last-activity time, so "it has no end" and "it is still
   * going" are two different questions and only the status answers the second. */
  const spanOf = (startMs: number, endMs: number, status: string) => {
    if (startMs <= 0) return undefined;
    const x = scale.toX(startMs);
    const open = LIVE_STATUS.has(status) || endMs <= 0;
    return { x, width: Math.max(MIN_BAR_PX, (open ? scale.width : scale.toX(endMs)) - x), open };
  };

  // Render order, depth and nesting are `hostedTree`'s, shared with the graph:
  // both pictures answer "what hangs off what" the same way or one of them is
  // lying. A sub session is drawn exactly like a subagent — from when it
  // branched to when it last did anything. It has no *end*, nothing closes a
  // session, but "still running, forever" was a worse lie than "this is how
  // far it got", and it made every sub session a bar running off the pane.
  for (const m of members) {
    const ends = m.kind === "sub_session" ? endOfSubSession(subSessions, m.id) : endOfAgent(agents, m.id);
    const span = spanOf(m.at, ends, m.status);
    lanes.push({
      agentId: m.id,
      kind: m.kind,
      label: m.label,
      status: m.status,
      bars: barsFor(m.id, m.at),
      depth: m.depth,
      span,
      anchor: span ? { x: span.x, parentAgentId: m.parent ?? rootId } : undefined,
      placed: span !== undefined,
      hasChildren: m.children > 0,
      detail: m.detail,
    });
  }

  return { lanes, gaps: scale.gaps, ticks, width: scale.width };
}
