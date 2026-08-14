# Session Timeline View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a timeline view to the session page that draws the main agent's transcript as a horizontal bar chart, with subagent and fork lanes hanging below it.

**Architecture:** One pure layout function (`lib/timeline.ts`) maps the main agent's transcript entries and the session's agent/fork rosters into pixel positions on a compressed-time axis; one component (`SessionTimeline.tsx`) renders that model as absolutely-positioned divs in a horizontal scroller. The session header gains a toggle that swaps the transcript pane for it via a `?view=timeline` search param. The server gains one field: `SessionDetail.forks`.

**Tech Stack:** Rust (axum, fluorite schemas), TypeScript, React 19, react-router-dom 7, TanStack Query, Tailwind 4, Vitest, Playwright. Package manager is **bun**, never npm.

**Spec:** `docs/superpowers/specs/2026-08-13-session-timeline-view-design.md`

## Global Constraints

- Work in the existing worktree at `.claude/worktrees/session-timeline` on branch `feat/session-timeline-view`. Do not touch the primary checkout.
- Never override git identity. No `-c user.name` / `-c user.email`. No Claude co-author trailers.
- Web deps install with `bun install`, never `npm ci`.
- A `.fl` schema edit MUST be followed by `make types` (`cd clients/web && bun run generate-types`) in the same commit.
- Playwright on macOS needs `TMPDIR=/tmp` or setup dies on a `sun_path` overflow.
- Wire field names are camelCase in TypeScript, snake_case in `.fl` and Rust. Fluorite does the conversion; a hand-written snake_case key in a TS object is silently ignored.
- Test ids that can appear more than once on a page must carry a discriminator (agent id), or Playwright strict mode fails.
- Layout constants, fixed for this feature: gap threshold `60_000` ms, collapsed gap width `20` px, minimum bar `6` px, maximum bar `320` px, target drawn width `2400` px, scale clamp `0.0005`–`0.02` px per ms.

---

### Task 1: Serve a session's forks on its detail document

**Files:**
- Modify: `crates/models/fluorite/session.fl` (the `SessionDetail` struct)
- Modify: `crates/server/src/http/handlers.rs` (`get_session`, around line 201)
- Test: `crates/server/src/http/handlers.rs` (module tests) or the nearest existing HTTP test module
- Regenerate: `clients/web/src/generated/session/sessionDetail.ts`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `SessionDetail.forks: ForkView[]` on the TypeScript side, where `ForkView` is `{ id: string; parent?: string; title?: string; status: string; createdAtMs: number }`. Task 3 and Task 5 consume it.

- [ ] **Step 1: Write the failing test**

Find the existing test module in `crates/server/src/http/handlers.rs` (or the sibling test file that exercises these handlers — search for `get_session` in `#[cfg(test)]` blocks). Add:

```rust
/// A session's forks belong on its own document, not only on the list row.
/// The timeline view reads one session and must not have to fetch the whole
/// session list to find out what branched off it.
#[tokio::test]
async fn get_session_reports_the_sessions_forks() {
    let (state, id) = session_with_one_fork().await;
    let detail = super::get_session_detail(&state, &id).await.unwrap();
    assert_eq!(detail.forks.len(), 1);
    assert_eq!(detail.forks[0].title.as_deref(), Some("Other migration"));
}
```

If no such harness exists, model `session_with_one_fork` on `forks_are_listed_without_loading_the_session` in `crates/server/src/sessions/supervisor.rs:1803` — it already builds a session, sends `SessionSupervisorCommand::ForksChanged` with a `ForkRow { id, parent: None, title: Some("Other migration".into()), status, created_at_ms }`, and waits for it to land. Reuse that setup and then call the HTTP handler path instead of reading the supervisor record.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-server get_session_reports_the_sessions_forks`
Expected: FAIL — `no field 'forks' on type 'SessionDetail'`.

- [ ] **Step 3: Add the field to the schema**

In `crates/models/fluorite/session.fl`, inside `struct SessionDetail`, immediately after the `agents` field:

```
    /// The conversations forked out of this session, so one read tells a
    /// client everything the session hosts.
    ///
    /// Separate from `agents` rather than mixed into it, because a fork is not
    /// a delegated task: it owes nobody a result and it never ends, so it has
    /// no end stamp for `SubAgentView` to carry. The server keeps the two
    /// apart for the same reason — `ForkRoster` is deliberately not a
    /// `SubAgentTree`.
    forks: Vec<ForkView>,
```

`ForkView` is already declared in this file, so no import is needed.

- [ ] **Step 4: Populate it in the handler**

In `crates/server/src/http/handlers.rs`, in the `SessionDetail { ... }` literal beginning at line 201, add after the `annotations` line:

```rust
        // The same rows `summary()` puts on a list entry, through the same
        // helper. The supervisor keeps them current whether or not the session
        // actor is loaded, so this costs no extra read.
        forks: rec.forks.iter().map(wire_fork).collect(),
```

- [ ] **Step 5: Regenerate the TypeScript types**

Run: `make types`
Expected: `clients/web/src/generated/session/sessionDetail.ts` now imports `ForkView` and declares `forks: ForkView[]`.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p horsie-server get_session_reports_the_sessions_forks`
Expected: PASS

- [ ] **Step 7: Check nothing else broke**

Run: `cargo build -p horsie-server && cd clients/web && bun run typecheck`
Expected: both clean. A missing struct field is a compile error in Rust, so any other `SessionDetail` construction site will surface here — `crates/server/src/routines/runner.rs` already sets `forks: vec![]` on a `SessionSummary` and may need the same on a detail.

- [ ] **Step 8: Commit**

```bash
git add crates/models/fluorite/session.fl crates/server/src/http/handlers.rs clients/web/src/generated
git commit -m "feat(sessions): report a session's forks on its detail document"
```

---

### Task 2: The time scale — collapsing idle gaps

**Files:**
- Create: `clients/web/src/lib/timeline.ts`
- Test: `clients/web/src/lib/timeline.test.ts`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `export interface Span { startMs: number; endMs: number }`
  - `export interface Scale { toX(ms: number): number; width: number; gaps: { x: number; elapsedMs: number }[] }`
  - `export function buildScale(spans: Span[]): Scale`
  - Constants `GAP_THRESHOLD_MS = 60_000`, `GAP_PX = 20`, `MIN_BAR_PX = 6`, `MAX_BAR_PX = 320`, `TARGET_PX = 2400`, `MIN_SCALE = 0.0005`, `MAX_SCALE = 0.02`.

  Tasks 3 and 4 consume `buildScale` and the constants.

- [ ] **Step 1: Write the failing test**

Create `clients/web/src/lib/timeline.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { buildScale, GAP_PX, MAX_BAR_PX, MIN_BAR_PX } from "./timeline";

const S = (startMs: number, endMs: number) => ({ startMs, endMs });

describe("buildScale", () => {
  it("places the first span at zero", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(1000)).toBe(0);
  });

  it("keeps a short gap proportional and collapses a long one", () => {
    // Two 10s spans. The first pair is separated by 10s (kept), the second by
    // an hour (collapsed to a fixed gutter).
    const kept = buildScale([S(0, 10_000), S(20_000, 30_000)]);
    const collapsed = buildScale([S(0, 10_000), S(3_610_000, 3_620_000)]);
    // A kept gap is drawn at the same scale as the spans around it, so the
    // second span starts twice as far along as the first one is wide.
    expect(kept.toX(20_000)).toBeCloseTo(2 * kept.toX(10_000), 5);
    expect(collapsed.toX(3_610_000)).toBeCloseTo(collapsed.toX(10_000) + GAP_PX, 5);
  });

  it("reports what each collapsed gap swallowed", () => {
    const s = buildScale([S(0, 10_000), S(3_610_000, 3_620_000)]);
    expect(s.gaps).toHaveLength(1);
    expect(s.gaps[0].elapsedMs).toBe(3_600_000);
  });

  it("clamps a span to the minimum and maximum bar width", () => {
    // One 1ms span cannot be scaled up to a visible size by the auto-scale
    // alone, and one 10-hour span must not be allowed to run away.
    const tiny = buildScale([S(0, 1)]);
    expect(tiny.toX(1) - tiny.toX(0)).toBe(MIN_BAR_PX);
    const huge = buildScale([S(0, 36_000_000)]);
    expect(huge.toX(36_000_000) - huge.toX(0)).toBe(MAX_BAR_PX);
  });

  it("clamps a moment before the start and after the end", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(0)).toBe(0);
    expect(s.toX(999_999)).toBe(s.width);
  });

  it("interpolates a moment inside a span", () => {
    const s = buildScale([S(0, 10_000)]);
    const half = s.toX(5_000);
    expect(half).toBeGreaterThan(0);
    expect(half).toBeLessThan(s.width);
    expect(half).toBeCloseTo(s.width / 2, 5);
  });

  it("returns an empty scale for no spans", () => {
    const s = buildScale([]);
    expect(s.width).toBe(0);
    expect(s.toX(12_345)).toBe(0);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd clients/web && bun run test:unit src/lib/timeline.test.ts`
Expected: FAIL — cannot resolve `./timeline`.

- [ ] **Step 3: Write the implementation**

Create `clients/web/src/lib/timeline.ts`:

```ts
/** Laying a session out along a horizontal axis.
 *
 * The axis is wall-clock order with the dead air taken out. A session is
 * mostly waiting — for a person to come back, for a long tool call — and an
 * honest linear axis spends 99% of its width on nothing. So: real elapsed time
 * between entries, up to a minute; past that, a fixed gutter labelled with what
 * it swallowed.
 *
 * The consequence, and the reason this is a `toX` function rather than a
 * multiplier: the drawn axis is monotone but NOT linear in time, so no caller
 * can convert a timestamp to a pixel by arithmetic. Every off-lane moment — a
 * subagent's spawn, a fork's branch point — goes through `toX`.
 */

/** A gap longer than this is dead air, not part of the work. */
export const GAP_THRESHOLD_MS = 60_000;
/** What a collapsed gap is drawn at, however long it really was. */
export const GAP_PX = 20;
/** Small enough to read as brief, big enough to still be a click target. */
export const MIN_BAR_PX = 6;
/** One forty-minute tool call must not push the session off the screen. */
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
 * Build the time→pixel map from the spans that will be drawn on the main lane.
 *
 * Spans are assumed sorted by `startMs` and non-overlapping, which is what the
 * transcript produces: an assistant message and the tools it issued are
 * consecutive, and parallel tool calls share a start but are laid out in issue
 * order.
 */
export function buildScale(spans: Span[]): Scale {
  if (spans.length === 0) {
    return { toX: () => 0, width: 0, gaps: [] };
  }

  // Scale so the *active* time — elapsed minus everything that will collapse —
  // fills the target width. Scaling on total elapsed instead would squeeze a
  // session with one overnight gap down to a smudge.
  let activeMs = 0;
  for (let i = 0; i < spans.length; i++) {
    activeMs += Math.max(0, spans[i].endMs - spans[i].startMs);
    if (i > 0) {
      const gap = spans[i].startMs - spans[i - 1].endMs;
      if (gap > 0 && gap <= GAP_THRESHOLD_MS) activeMs += gap;
    }
  }
  const scale = activeMs > 0 ? clamp(TARGET_PX / activeMs, MIN_SCALE, MAX_SCALE) : MIN_SCALE;

  // Breakpoints: (ms, px) pairs in increasing order of both. Between two
  // consecutive points the map is linear, which is what makes `toX` an
  // interpolation rather than a special case per kind of interval.
  const ms: number[] = [];
  const px: number[] = [];
  const gaps: { x: number; elapsedMs: number }[] = [];
  let x = 0;

  const push = (atMs: number, atPx: number) => {
    // A zero-duration span (a user message) would otherwise put two points at
    // the same ms with different px, and interpolation would divide by zero.
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
    // Clamped, which is where the drawing stops being literally true — a bar
    // at MAX_BAR_PX is marked in the UI, and the tooltip carries the real
    // number. A zero-duration span still gets MIN_BAR_PX so it is clickable.
    x += clamp(duration * scale, MIN_BAR_PX, MAX_BAR_PX);
    push(spans[i].endMs, x);
  }

  const width = x;
  const toX = (at: number): number => {
    if (at <= ms[0]) return 0;
    if (at >= ms[ms.length - 1]) return width;
    // Linear scan: a session has hundreds of breakpoints, not millions, and
    // this runs once per off-lane agent rather than per frame.
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd clients/web && bun run test:unit src/lib/timeline.test.ts`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/lib/timeline.ts clients/web/src/lib/timeline.test.ts
git commit -m "feat(web): a compressed-time scale for the session timeline"
```

---

### Task 3: The lane model — bars, spans and anchors

**Files:**
- Modify: `clients/web/src/lib/timeline.ts`
- Test: `clients/web/src/lib/timeline.test.ts`

**Interfaces:**
- Consumes: `buildScale`, `Span`, `Scale` and the constants from Task 2. `TranscriptItem` / `RenderedMessage` / `RenderedToolCall` from `../hooks/useSessionStream`. `SubAgentView` / `ForkView` from `../api/types`. `forkTree` from `./forkTree`. `isAskCall` from `./askUser`.
- Produces:
  - `export type BarKind = "user" | "assistant" | "thinking" | "tool" | "ask" | "compaction"`
  - `export type LaneKind = "main" | "subagent" | "fork"`
  - `export interface Bar { key: string; kind: BarKind; x: number; width: number; entryId: string; title: string; detail: string; live?: boolean }`
  - `export interface Lane { agentId: string; kind: LaneKind; label: string; status: string; depth: number; bars: Bar[]; span?: { x: number; width: number; open: boolean }; anchor?: { x: number; parentAgentId: string }; placed: boolean }`
  - `export interface Timeline { lanes: Lane[]; gaps: { x: number; elapsedMs: number }[]; ticks: { x: number; label: string }[]; width: number }`
  - `export function buildTimeline(items: TranscriptItem[], agents: SubAgentView[], forks: ForkView[], nowMs: number): Timeline`

  Task 4 renders `Timeline`. Task 5 wires the inputs.

- [ ] **Step 1: Write the failing test**

Append to `clients/web/src/lib/timeline.test.ts`:

```ts
import { buildTimeline } from "./timeline";
import type { TranscriptItem } from "../hooks/useSessionStream";
import type { ForkView, SubAgentView } from "../api/types";

const msg = (
  id: string,
  role: "User" | "Assistant",
  createdAtMs: number,
  extra: Partial<{
    startedAtMs: number;
    text: string;
    thinking: string[];
    toolCalls: { id: string; name: string; endedAtMs?: number }[];
  }> = {},
): TranscriptItem => ({
  kind: "message",
  value: {
    id,
    role,
    text: extra.text ?? "",
    thinking: extra.thinking ?? [],
    toolCalls: (extra.toolCalls ?? []).map((t) => ({
      id: t.id,
      name: t.name,
      input: {},
      running: t.endedAtMs === undefined,
      endedAtMs: t.endedAtMs,
      hooks: [],
    })),
    subagentResults: [],
    createdAtMs,
    startedAtMs: extra.startedAtMs,
  },
});

const agent = (o: Partial<SubAgentView> & { id: string }): SubAgentView => ({
  parent: undefined,
  label: o.id,
  depth: 0,
  status: "completed",
  spawnedAtMs: 0,
  endedAtMs: 0,
  ...o,
});

const fork = (o: Partial<ForkView> & { id: string }): ForkView => ({
  status: "idle",
  createdAtMs: 0,
  ...o,
});

// A minimal session: one user message, one assistant turn that thinks and then
// calls a tool, and the tool's answer.
const SESSION: TranscriptItem[] = [
  msg("m1", "User", 1_000),
  msg("m2", "Assistant", 5_000, {
    startedAtMs: 2_000,
    thinking: ["hmm"],
    toolCalls: [{ id: "t1", name: "Bash", endedAtMs: 12_000 }],
  }),
];

describe("buildTimeline", () => {
  it("draws one bar per entry on the main lane, in order", () => {
    const t = buildTimeline(SESSION, [agent({ id: "main" })], [], 20_000);
    const main = t.lanes[0];
    expect(main.kind).toBe("main");
    expect(main.bars.map((b) => b.kind)).toEqual(["user", "thinking", "tool"]);
    // Monotone left to right.
    const xs = main.bars.map((b) => b.x);
    expect([...xs].sort((a, b) => a - b)).toEqual(xs);
  });

  it("gives every bar something to scroll the transcript to", () => {
    const t = buildTimeline(SESSION, [agent({ id: "main" })], [], 20_000);
    for (const bar of t.lanes[0].bars) expect(bar.entryId).toBeTruthy();
  });

  it("puts a tick at each turn start", () => {
    const t = buildTimeline(SESSION, [agent({ id: "main" })], [], 20_000);
    expect(t.ticks).toHaveLength(1);
    expect(t.ticks[0].x).toBe(0);
  });

  it("hangs a subagent under the main lane, anchored at its spawn", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" }), agent({ id: "s1", label: "Explore", spawnedAtMs: 6_000, endedAtMs: 11_000 })],
      [],
      20_000,
    );
    const sub = t.lanes.find((l) => l.agentId === "s1");
    expect(sub?.kind).toBe("subagent");
    expect(sub?.span?.open).toBe(false);
    // Spawned inside the tool call, so it sits to the right of the tool's start.
    const tool = t.lanes[0].bars.find((b) => b.kind === "tool");
    expect(sub!.anchor!.x).toBeGreaterThanOrEqual(tool!.x);
    expect(sub!.anchor!.parentAgentId).toBe("main");
  });

  it("leaves a running subagent's span open", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" }), agent({ id: "s1", spawnedAtMs: 6_000, endedAtMs: 0, status: "running" })],
      [],
      20_000,
    );
    expect(t.lanes.find((l) => l.agentId === "s1")?.span?.open).toBe(true);
  });

  it("leaves every fork's span open, because a conversation never ends", () => {
    const t = buildTimeline(SESSION, [agent({ id: "main" })], [fork({ id: "f1", createdAtMs: 6_000 })], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "f1");
    expect(lane?.kind).toBe("fork");
    expect(lane?.span?.open).toBe(true);
  });

  it("nests a fork of a fork and anchors it to its parent", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" })],
      [fork({ id: "f1", createdAtMs: 6_000 }), fork({ id: "f2", parent: "f1", createdAtMs: 8_000 })],
      20_000,
    );
    const child = t.lanes.find((l) => l.agentId === "f2");
    expect(child?.depth).toBe(1);
    expect(child?.anchor?.parentAgentId).toBe("f1");
  });

  it("shows a fork whose parent was deleted, at the top level", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" })],
      [fork({ id: "f2", parent: "gone", createdAtMs: 8_000 })],
      20_000,
    );
    const lane = t.lanes.find((l) => l.agentId === "f2");
    expect(lane).toBeDefined();
    expect(lane?.depth).toBe(0);
  });

  it("shows a subagent whose parent was never delivered, at the top level", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" }), agent({ id: "s9", parent: "missing", spawnedAtMs: 6_000, endedAtMs: 7_000 })],
      [],
      20_000,
    );
    const lane = t.lanes.find((l) => l.agentId === "s9");
    expect(lane?.anchor?.parentAgentId).toBe("main");
  });

  it("marks an agent with no usable stamps as unplaced rather than dropping it", () => {
    const t = buildTimeline(
      SESSION,
      [agent({ id: "main" }), agent({ id: "old", label: "ancient", spawnedAtMs: 0, endedAtMs: 0 })],
      [],
      20_000,
    );
    const lane = t.lanes.find((l) => l.agentId === "old");
    expect(lane?.placed).toBe(false);
    expect(lane?.span).toBeUndefined();
  });

  it("does not draw a subagent result as a bar on its parent's lane", () => {
    // It already has a lane of its own; drawing it twice says the work
    // happened twice.
    const withResult: TranscriptItem[] = [
      ...SESSION,
      {
        kind: "message",
        value: {
          id: "m3",
          role: "User",
          text: "",
          thinking: [],
          toolCalls: [],
          subagentResults: [
            { subagentId: "s1", label: "Explore", status: "completed", text: "done", spawnedAtMs: 6_000, endedAtMs: 11_000 },
          ],
          createdAtMs: 13_000,
        },
      },
    ];
    const t = buildTimeline(withResult, [agent({ id: "main" })], [], 20_000);
    expect(t.lanes[0].bars.every((b) => b.kind !== "user" || b.entryId !== "m3")).toBe(true);
  });

  it("survives a session with nothing in it", () => {
    const t = buildTimeline([], [agent({ id: "main" })], [], 20_000);
    expect(t.width).toBe(0);
    expect(t.lanes[0].bars).toEqual([]);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd clients/web && bun run test:unit src/lib/timeline.test.ts`
Expected: FAIL — `buildTimeline` is not exported.

- [ ] **Step 3: Write the implementation**

Append to `clients/web/src/lib/timeline.ts`:

```ts
import type { ForkView, SubAgentView } from "../api/types";
import type { RenderedMessage, TranscriptItem } from "../hooks/useSessionStream";
import { isAskCall } from "./askUser";
import { forkTree } from "./forkTree";
import { MAIN_AGENT } from "../api/client";

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
  /** False when nothing on this agent could be placed on the axis. */
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
  const h = Math.floor(ms / 3_600_000);
  return `${h}h ${Math.round((ms % 3_600_000) / 60_000)}m`;
}

function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/**
 * Flatten one message into the things the timeline draws.
 *
 * The time rules are `transcriptSegments.ts`'s, not a second set: an assistant
 * message spans the provider call that produced it, and its tool calls were
 * issued at the end of that call and each ended whenever its result landed.
 * A tool result carries no timestamps of its own — `ToolResultPart` has none —
 * so this is the only way the duration can be known.
 *
 * A subagent result is deliberately not drawn. It arrives inside a user
 * message, and it already has a lane.
 */
function entriesOf(m: RenderedMessage, nowMs: number): Entry[] {
  const out: Entry[] = [];
  const ended = m.createdAtMs ?? 0;
  const began = m.startedAtMs ?? ended;

  if (m.role === "User") {
    // Only a real message the person sent. One carrying nothing but a
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
 * `nowMs` is passed rather than read so this stays pure and testable — and so
 * a still-running bar is measured against the same instant as everything else
 * in one layout pass.
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
        entryId: String(item.value.seq),
        startMs: item.value.atMs,
        endMs: item.value.atMs,
        title: "Conversation compacted",
        live: false,
        turnStart: false,
      });
    }
    // A `fork` item is not drawn on this lane: the fork has a lane of its own,
    // and the branch arrow is what says where it came from. A `notice` is a
    // hook record, which is not work the session spent time on.
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

  // --- Lanes -------------------------------------------------------------

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

  /** A span, or nothing when the agent has no stamp to place it by. */
  const spanOf = (startMs: number, endMs: number) => {
    if (startMs <= 0) return undefined;
    const x = scale.toX(startMs);
    const open = endMs <= 0;
    return { x, width: Math.max(MIN_BAR_PX, (open ? scale.width : scale.toX(endMs)) - x), open };
  };

  const subIds = new Set(agents.map((a) => a.id));
  for (const a of agents) {
    if (a.id === mainId) continue;
    const span = spanOf(a.spawnedAtMs, a.endedAtMs);
    lanes.push({
      agentId: a.id,
      kind: "subagent",
      label: a.label ?? a.agentType ?? "subagent",
      status: a.status,
      // A parent nobody holds is the same as no parent — deleting or never
      // delivering one must not hide the child. `forkTree` learned this on the
      // same journal-derived data.
      depth: a.parent && subIds.has(a.parent) ? a.depth : 0,
      bars: [],
      span,
      anchor: span
        ? { x: span.x, parentAgentId: a.parent && subIds.has(a.parent) ? a.parent : mainId }
        : undefined,
      placed: span !== undefined,
    });
  }

  // `forkTree` already turns a flat, parent-linked list into render order with
  // a depth per row, handles an orphaned parent and refuses to drop a cycle.
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd clients/web && bun run test:unit src/lib/timeline.test.ts`
Expected: PASS, all tests.

- [ ] **Step 5: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: clean. Confirm `MAIN_AGENT` is exported from `src/api/client.ts` (it is — `SessionView.tsx` imports it) and `isAskCall` from `src/lib/askUser.ts`.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/lib/timeline.ts clients/web/src/lib/timeline.test.ts
git commit -m "feat(web): lay a session's agents out as timeline lanes"
```

---

### Task 4: Render the timeline

**Files:**
- Create: `clients/web/src/components/SessionTimeline.tsx`
- Test: `clients/web/src/components/SessionTimeline.test.tsx`

**Interfaces:**
- Consumes: `Timeline`, `Lane`, `Bar`, `BarKind` from `../lib/timeline`.
- Produces:
  ```ts
  export function SessionTimeline(props: {
    timeline: Timeline;
    onSelectEntry: (entryId: string) => void;
    onSelectAgent: (agentId: string) => void;
  }): React.ReactElement
  ```
  Task 5 mounts it.

- [ ] **Step 1: Write the failing test**

Create `clients/web/src/components/SessionTimeline.test.tsx`:

```tsx
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SessionTimeline } from "./SessionTimeline";
import type { Timeline } from "../lib/timeline";

const TIMELINE: Timeline = {
  width: 400,
  gaps: [{ x: 120, elapsedMs: 3_600_000 }],
  ticks: [{ x: 0, label: "09:12" }],
  lanes: [
    {
      agentId: "main",
      kind: "main",
      label: "main agent",
      status: "idle",
      depth: 0,
      placed: true,
      bars: [
        { key: "b1", kind: "user", x: 0, width: 20, entryId: "m1", title: "hi", detail: "09:12" },
        { key: "b2", kind: "tool", x: 24, width: 80, entryId: "m2", title: "Bash", detail: "12.4s" },
      ],
    },
    {
      agentId: "s1",
      kind: "subagent",
      label: "Explore",
      status: "completed",
      depth: 0,
      placed: true,
      bars: [],
      span: { x: 30, width: 60, open: false },
      anchor: { x: 30, parentAgentId: "main" },
    },
    {
      agentId: "old",
      kind: "subagent",
      label: "ancient",
      status: "completed",
      depth: 0,
      placed: false,
      bars: [],
    },
  ],
};

describe("SessionTimeline", () => {
  it("draws a lane per agent", () => {
    render(<SessionTimeline timeline={TIMELINE} onSelectEntry={vi.fn()} onSelectAgent={vi.fn()} />);
    expect(screen.getByTestId("timeline-lane-main")).toBeTruthy();
    expect(screen.getByTestId("timeline-lane-s1")).toBeTruthy();
  });

  it("hands a clicked bar's entry back", async () => {
    const onSelectEntry = vi.fn();
    render(<SessionTimeline timeline={TIMELINE} onSelectEntry={onSelectEntry} onSelectAgent={vi.fn()} />);
    await userEvent.click(screen.getByTestId("timeline-bar-m2"));
    expect(onSelectEntry).toHaveBeenCalledWith("m2");
  });

  it("hands a clicked lane's agent back", async () => {
    const onSelectAgent = vi.fn();
    render(<SessionTimeline timeline={TIMELINE} onSelectEntry={vi.fn()} onSelectAgent={onSelectAgent} />);
    await userEvent.click(screen.getByTestId("timeline-span-s1"));
    expect(onSelectAgent).toHaveBeenCalledWith("s1");
  });

  it("says what a collapsed gap swallowed", () => {
    render(<SessionTimeline timeline={TIMELINE} onSelectEntry={vi.fn()} onSelectAgent={vi.fn()} />);
    expect(screen.getByTestId("timeline-gap").textContent).toContain("1h");
  });

  it("keeps an unplaced agent visible, outside the axis", () => {
    render(<SessionTimeline timeline={TIMELINE} onSelectEntry={vi.fn()} onSelectAgent={vi.fn()} />);
    const lane = screen.getByTestId("timeline-lane-old");
    expect(lane.getAttribute("data-placed")).toBe("false");
    expect(screen.queryByTestId("timeline-span-old")).toBeNull();
  });

  it("says so when there is nothing to draw", () => {
    render(
      <SessionTimeline
        timeline={{ lanes: [], gaps: [], ticks: [], width: 0 }}
        onSelectEntry={vi.fn()}
        onSelectAgent={vi.fn()}
      />,
    );
    expect(screen.getByTestId("timeline-empty")).toBeTruthy();
  });
});
```

If `@testing-library/user-event` is not in `package.json`, add it: `cd clients/web && bun add -d @testing-library/user-event`. Check first — other component tests may already use `fireEvent` from `@testing-library/react` instead, in which case follow them and use `fireEvent.click`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd clients/web && bun run test:unit src/components/SessionTimeline.test.tsx`
Expected: FAIL — cannot resolve `./SessionTimeline`.

- [ ] **Step 3: Write the component**

Create `clients/web/src/components/SessionTimeline.tsx`:

```tsx
import type { Bar, BarKind, Lane, Timeline } from "../lib/timeline";
import { MAX_BAR_PX } from "../lib/timeline";
import { cn } from "../lib/cn";

/** The session's shape, drawn along one axis.
 *
 * Plain positioned divs rather than SVG: `WorkflowGraph` is SVG because it has
 * to route edges around ranks, and this has nothing to route. Divs come with
 * hover, focus and keyboard activation already working, and a few hundred of
 * them is not a rendering problem.
 *
 * Every lane shares one scroller so they cannot drift out of alignment, and
 * the label gutter is sticky inside it so a lane stays identifiable however
 * far right you have scrolled.
 */

const LANE_H = 34;
const BAR_H = 24;
const GUTTER_W = 148;

/** Lit by the same lamps as the rest of the console, so all three skins work
 * without a fourth set of colours. */
const BAR_CLASS: Record<BarKind, string> = {
  user: "bg-[var(--accent-quiet)] border-[var(--accent)]",
  assistant: "bg-lamp-ok-quiet border-[var(--lamp-ok)]",
  thinking: "bg-raised border-[var(--rule-strong)]",
  tool: "bg-amber-quiet border-amber",
  ask: "bg-[var(--attention-quiet)] border-[var(--attention)]",
  compaction: "bg-transparent border-dashed border-[var(--rule-strong)]",
};

export function SessionTimeline({
  timeline,
  onSelectEntry,
  onSelectAgent,
}: {
  timeline: Timeline;
  /** A bar on the main lane: go read that entry. */
  onSelectEntry: (entryId: string) => void;
  /** A lane: go open that agent. */
  onSelectAgent: (agentId: string) => void;
}) {
  const placed = timeline.lanes.filter((l) => l.placed);
  const unplaced = timeline.lanes.filter((l) => !l.placed);

  if (timeline.lanes.length === 0) {
    return (
      <div className="flex h-full items-center justify-center" data-testid="timeline-empty">
        <p className="max-w-sm text-center text-sm leading-relaxed text-dim">
          Nothing has happened in this session yet. The timeline draws itself as
          the agent works.
        </p>
      </div>
    );
  }

  // Subagents are work inside a turn; forks are other conversations. The
  // divider is the same distinction `SubAgentCard` and `ForkMarker` draw.
  const firstForkAt = placed.findIndex((l) => l.kind === "fork");

  return (
    <div className="h-full overflow-auto" data-testid="session-timeline">
      <div className="relative" style={{ width: GUTTER_W + timeline.width + 48 }}>
        {/* Collapsed idle stretches, behind everything. */}
        {timeline.gaps.map((g) => (
          <div
            key={g.x}
            data-testid="timeline-gap"
            title={`${humanGap(g.elapsedMs)} with nothing happening`}
            className="absolute top-0 bottom-0 flex items-start justify-center bg-[repeating-linear-gradient(135deg,var(--rule)_0_1px,transparent_1px_5px)] pt-1"
            style={{ left: GUTTER_W + g.x, width: 20 }}
          >
            <span className="legend rotate-90 whitespace-nowrap text-[0.5625rem]">
              {humanGap(g.elapsedMs)}
            </span>
          </div>
        ))}

        {/* Turn starts. */}
        <div className="relative h-5">
          {timeline.ticks.map((t) => (
            <span
              key={t.x}
              className="legend absolute top-0 text-[0.5625rem]"
              style={{ left: GUTTER_W + t.x }}
            >
              {t.label}
            </span>
          ))}
        </div>

        {placed.map((lane, i) => (
          <div key={lane.agentId}>
            {i === firstForkAt && firstForkAt > 0 && (
              <div className="flex items-center gap-3 py-2 pl-3">
                <span className="legend whitespace-nowrap">forked conversations</span>
                <span className="h-px flex-1 bg-[var(--rule)]" />
              </div>
            )}
            <LaneRow lane={lane} onSelectEntry={onSelectEntry} onSelectAgent={onSelectAgent} />
          </div>
        ))}

        {unplaced.length > 0 && (
          <div className="mt-3 border-t pt-2">
            <p className="legend pl-3">
              not on the timeline — nothing recorded when these ran
            </p>
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
        className="sticky left-0 z-10 shrink-0 truncate bg-panel pr-2 pl-3"
        style={{ width: GUTTER_W, paddingLeft: 12 + lane.depth * 12 }}
      >
        {lane.kind === "main" ? (
          <span className="text-xs font-medium text-legend">{lane.label}</span>
        ) : (
          <button
            type="button"
            className="truncate text-xs text-faint hover:text-legend"
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
              "absolute top-1/2 -translate-y-1/2 rounded-[var(--radius-chip)] border transition-colors",
              lane.kind === "fork"
                ? "border-[var(--accent)] bg-[var(--accent-quiet)]"
                : "border-[var(--rule-strong)] bg-raised",
              lane.status === "failed" && "!border-red !bg-red-quiet",
              // An open span has no known end, so it fades out rather than
              // claiming one.
              lane.span.open && "opacity-70",
            )}
            style={{ left: lane.span.x, width: lane.span.width, height: BAR_H - 6 }}
          />
        )}
      </div>
    </div>
  );
}

function BarView({ bar, onSelect }: { bar: Bar; onSelect: (entryId: string) => void }) {
  // A bar drawn at the cap is shorter than the truth. Marked, so the picture
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
        "absolute top-1/2 -translate-y-1/2 rounded-[var(--radius-chip)] border transition-[filter] hover:brightness-110 focus-visible:outline focus-visible:outline-2",
        BAR_CLASS[bar.kind],
        bar.live && "animate-pulse",
        capped && "border-r-4 border-r-dashed",
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd clients/web && bun run test:unit src/components/SessionTimeline.test.tsx`
Expected: PASS.

If a CSS variable used above does not exist in this codebase, grep `clients/web/src/index.css` (or wherever the skin variables live) for the real names and substitute — `--accent`, `--attention`, `--lamp-ok` are the ones most likely to differ. Do not invent new variables; use existing ones.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/components/SessionTimeline.tsx clients/web/src/components/SessionTimeline.test.tsx
git commit -m "feat(web): render the session timeline"
```

---

### Task 5: Wire it into the session page

**Files:**
- Modify: `clients/web/src/pages/SessionView.tsx`
- Modify: `clients/web/src/components/Transcript.tsx`

**Interfaces:**
- Consumes: `buildTimeline` (Task 3), `SessionTimeline` (Task 4), `SessionDetail.forks` (Task 1).
- Produces: the `?view=timeline` search param, and a `data-entry-id` attribute on every rendered message.

- [ ] **Step 1: Add scroll anchors to the transcript**

In `clients/web/src/components/Transcript.tsx`, find the two elements carrying `data-testid="message"` (around lines 127 and 155) and add a `data-entry-id` attribute set to the message's id — the same value already used as the React key. For example:

```tsx
      data-testid="message"
      data-entry-id={msg.id}
```

Do this for both the user-turn and assistant-turn elements. Nothing else changes.

- [ ] **Step 2: Teach `seek` about entries**

In `clients/web/src/pages/SessionView.tsx`, widen the `seek` function's parameter and add a case. Change the signature to:

```tsx
  /** Scroll to a transcript entry, a compaction boundary by seq, or either end. */
  const seek = (target: number | string | "start" | "end") => {
```

and add, before the existing compaction-divider query at the end of the body:

```tsx
    // An entry id rather than a seq. A timeline bar seeks by the id the
    // message carries, because that is the only identity a message has.
    if (typeof target === "string") {
      const el = scrollRef.current?.querySelector(`[data-entry-id="${CSS.escape(target)}"]`);
      // Absent means it has been paged out. `seek("start")` has the same
      // limitation and answers it the same way: go as far back as has loaded
      // and let the scroll-back handler fetch the rest.
      if (el) el.scrollIntoView({ behavior: "smooth", block: "center" });
      else scrollRef.current?.scrollTo({ top: 0, behavior: "smooth" });
      return;
    }
```

Note the existing `"start"` / `"end"` cases are also strings, so they must be handled *before* this block. Check the order: the function already returns early for both, so put this after them.

- [ ] **Step 3: Add the view toggle**

In `SessionView.tsx`, import `useSearchParams` from `react-router-dom`, `GitBranch` (or another suitable icon) from `lucide-react`, `SessionTimeline`, and `buildTimeline`. Then inside `SessionView`:

```tsx
  const [searchParams, setSearchParams] = useSearchParams();
  // In the URL rather than component state: a view of a session is a thing you
  // send someone, and it should survive a reload.
  const timelineOpen = searchParams.get("view") === "timeline";
  const setTimelineOpen = (on: boolean) =>
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (on) next.set("view", "timeline");
        else next.delete("view");
        return next;
      },
      { replace: true },
    );
```

and the button, placed immediately after `<SessionTitle … />` in the header:

```tsx
            {/* Beside the title rather than in the key cluster on the right:
                this changes what you are looking at, and that cluster is for
                acting on what you are already looking at. */}
            <button
              className={cn("key-icon", timelineOpen && "bg-raised !text-legend")}
              onClick={() => setTimelineOpen(!timelineOpen)}
              aria-pressed={timelineOpen}
              title={timelineOpen ? "Show the transcript" : "Show the timeline"}
              aria-label="Toggle the session timeline"
              data-testid="timeline-toggle"
            >
              <GitBranch size={15} aria-hidden />
            </button>
```

- [ ] **Step 4: Build and render the timeline**

Still in `SessionView.tsx`, above the return:

```tsx
  // Rebuilt whenever the transcript or the roster moves. `Date.now()` is read
  // here rather than inside the builder so one layout pass measures every
  // still-running bar against the same instant.
  const timeline = useMemo(
    () => buildTimeline(stream.items, detail?.agents ?? [], detail?.forks ?? [], Date.now()),
    [stream.items, detail?.agents, detail?.forks],
  );
```

Then wrap the transcript scroller so the timeline replaces it. Find the `<div ref={scrollRef} onScroll={onScroll} data-testid="transcript-scroll" …>` block and render the timeline instead when `timelineOpen`:

```tsx
          {timelineOpen ? (
            <div className="flex-1 overflow-hidden">
              <SessionTimeline
                timeline={timeline}
                onSelectEntry={(entryId) => {
                  // Reading an entry means reading the transcript. Switch back
                  // first, then seek — the anchor does not exist until it is
                  // rendered, so the seek waits a frame.
                  setTimelineOpen(false);
                  requestAnimationFrame(() => seek(entryId));
                }}
                onSelectAgent={(agentId) =>
                  navigate(`/sessions/${id}/agents/${agentId}`)
                }
              />
            </div>
          ) : (
            <div ref={scrollRef} onScroll={onScroll} data-testid="transcript-scroll" …>
              {/* unchanged */}
            </div>
          )}
```

Keep the existing scroller's contents exactly as they are; only the conditional wrapper is new.

- [ ] **Step 5: Typecheck and run the unit suite**

Run: `cd clients/web && bun run typecheck && bun run test:unit`
Expected: both clean.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/pages/SessionView.tsx clients/web/src/components/Transcript.tsx
git commit -m "feat(web): toggle the session page between transcript and timeline"
```

---

### Task 6: End-to-end coverage

**Files:**
- Create: `clients/web/e2e/x-session-timeline.spec.ts`

**Interfaces:**
- Consumes: everything above, plus the existing e2e harness in `clients/web/e2e/harness.ts`, `fixtures.ts` and `helpers.ts`.
- Produces: nothing further.

- [ ] **Step 1: Read the existing subagent spec**

Read `clients/web/e2e/s-subagent-results.spec.ts` in full. It already drives a session that spawns a subagent against the mock LLM, which is exactly the fixture this needs. Copy its setup rather than inventing a new one.

- [ ] **Step 2: Write the spec**

Create `clients/web/e2e/x-session-timeline.spec.ts`, following the imports and fixture usage of `s-subagent-results.spec.ts`:

```ts
import { expect, test } from "./fixtures";

test("the timeline draws the session and clicks through to a subagent", async ({ page }) => {
  // …the same setup s-subagent-results.spec.ts uses to run a turn that spawns
  // a subagent and lets it finish…

  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();

  // The main lane, and at least one bar on it.
  const main = page.locator('[data-testid^="timeline-lane-"][data-kind="main"]');
  await expect(main).toHaveCount(1);
  await expect(page.locator('[data-testid^="timeline-bar-"]').first()).toBeVisible();

  // The subagent has a lane of its own, and it opens that agent.
  const sub = page.locator('[data-testid^="timeline-lane-"][data-kind="subagent"]').first();
  await expect(sub).toBeVisible();
  await sub.locator('[data-testid^="timeline-span-"]').click();
  await expect(page).toHaveURL(/\/agents\//);
});

test("a bar goes back to the transcript at that entry", async ({ page }) => {
  // …same setup…

  await page.getByTestId("timeline-toggle").click();
  await page.locator('[data-testid^="timeline-bar-"]').first().click();
  // Back on the transcript.
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
});
```

Fill the elided setup from the spec you read in Step 1 — do not leave it elided.

- [ ] **Step 3: Run the spec**

Run: `cd clients/web && TMPDIR=/tmp bun run test:e2e x-session-timeline`
Expected: PASS. The `TMPDIR` is required on macOS — the default one overflows `sun_path` during Playwright's setup.

- [ ] **Step 4: Commit**

```bash
git add clients/web/e2e/x-session-timeline.spec.ts
git commit -m "test(web): cover the session timeline end to end"
```

---

### Task 7: Verify the whole workspace and open the PR

**Files:** none.

- [ ] **Step 1: Format and lint the Rust side**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
Expected: clean. Run `fmt` *before* `clippy` — clippy reports formatting-adjacent lints that vanish after a format, and fixing them by hand wastes a cycle.

- [ ] **Step 2: Run the full Rust test suite**

Run: `cargo test --workspace`
Expected: PASS. The workspace, not `-p horsie-server` — the e2e crate hits these routes too, and a single-crate green is a false one.

- [ ] **Step 3: Run the full web suite**

Run: `cd clients/web && bun run typecheck && bun run test:unit && TMPDIR=/tmp bun run test:e2e`
Expected: all PASS.

- [ ] **Step 4: Read the PR template**

Run: `cat .github/pull_request_template.md`
Fill every section it asks for. Do not write a body from memory of what PR bodies look like.

- [ ] **Step 5: Re-read the spec against the full diff**

Run: `git diff origin/main...HEAD`

Read the spec and the whole diff together. Per-task review cannot catch a task built from a wrong brief; this is the pass that can. Fix anything that has drifted.

- [ ] **Step 6: Push and open the PR**

```bash
git push -u origin feat/session-timeline-view
gh pr create --title "feat(web): a timeline view of a session" --body-file <the body you wrote>
```

Body style: one long line per paragraph or bullet, never hard-wrapped. Short and plain. No AI attribution anywhere.

- [ ] **Step 7: Watch the checks to green**

Poll by commit SHA, not by `gh pr checks` alone: a fresh PR briefly has only the CLA check, and after a merge `gh pr checks` replays the previous run. A silent watcher is not a pass.

Do **not** enable auto-merge. A green PR is the finish line.

## Self-review notes

Spec coverage: the toggle and `?view=` (Task 5), the lane model and compressed time (Tasks 2–3), rendering (Task 4), click-through and `data-entry-id` (Task 5), the server change (Task 1), unit and e2e testing (Tasks 2–4, 6). The "where the picture lies" markers are Task 4's `capped` edge and the gap gutter.

Type consistency: `buildScale` → `Scale.toX` is used by `buildTimeline` only; `buildTimeline` → `Timeline` is consumed by `SessionTimeline`'s single prop. `Bar.entryId` is a required string everywhere, including on a compaction bar, where it holds the boundary seq as a string.
