import { describe, expect, it } from "vitest";
import type { ForkView, SubAgentView } from "../api/types";
import type { TranscriptItem } from "../hooks/useSessionStream";
import {
  buildScale,
  buildTimeline,
  GAP_PX,
  MIN_BAR_PX,
  TARGET_LONGEST_PX,
} from "./timeline";

const S = (startMs: number, endMs: number) => ({ startMs, endMs });

describe("buildScale", () => {
  it("places the first span at zero", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(1000)).toBe(0);
  });

  it("draws the longest span at the target width, whatever the session's pace", () => {
    // The bug this pins: the old rule scaled the session's *total* to a fixed
    // width and then clamped the scale, so a session that finished in three
    // seconds drew as a hundred-pixel smudge.
    const quick = buildScale([S(0, 800), S(900, 3_000)]);
    const slow = buildScale([S(0, 800_000), S(900_000, 3_000_000)]);
    expect(quick.toX(3_000) - quick.toX(900)).toBeCloseTo(TARGET_LONGEST_PX, 5);
    expect(slow.toX(3_000_000) - slow.toX(900_000)).toBeCloseTo(TARGET_LONGEST_PX, 5);
  });

  it("keeps every other span in proportion to the longest", () => {
    const s = buildScale([S(0, 1_000), S(1_000, 3_000)]);
    const first = s.toX(1_000) - s.toX(0);
    const second = s.toX(3_000) - s.toX(1_000);
    expect(second).toBeCloseTo(2 * first, 5);
  });

  it("never draws a gap wider than the collapsed gutter", () => {
    // Waiting must not outdraw working. At a quick session's scale a
    // proportional fifty-second pause would be wider than every bar together.
    const kept = buildScale([S(0, 1_000), S(51_000, 52_000)]);
    expect(kept.toX(51_000) - kept.toX(1_000)).toBe(GAP_PX);
    const collapsed = buildScale([S(0, 1_000), S(3_601_000, 3_602_000)]);
    expect(collapsed.toX(3_601_000) - collapsed.toX(1_000)).toBe(GAP_PX);
  });

  it("keeps a gap short enough to draw in proportion", () => {
    const s = buildScale([S(0, 240_000), S(240_100, 240_200)]);
    // 100ms at 240px per 240_000ms is a tenth of a pixel — under the cap, so
    // it stays proportional rather than jumping to the gutter width.
    expect(s.toX(240_100) - s.toX(240_000)).toBeLessThan(GAP_PX);
  });

  it("reports what each collapsed gap swallowed", () => {
    const s = buildScale([S(0, 10_000), S(3_610_000, 3_620_000)]);
    expect(s.gaps).toHaveLength(1);
    expect(s.gaps[0].elapsedMs).toBe(3_600_000);
  });

  it("gives an instantaneous span a clickable minimum", () => {
    const s = buildScale([S(0, 0)]);
    expect(s.toX(0)).toBe(0);
    expect(s.width).toBe(MIN_BAR_PX);
  });

  it("survives a session in which nothing took any time at all", () => {
    // Only user messages: there is no longest span to be in proportion to, so
    // the scale is zero and every bar falls to the minimum.
    const s = buildScale([S(1_000, 1_000), S(2_000, 2_000)]);
    expect(s.width).toBe(2 * MIN_BAR_PX);
  });

  it("clamps a moment before the start and after the end", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(0)).toBe(0);
    expect(s.toX(999_999)).toBe(s.width);
  });

  it("interpolates a moment inside a span", () => {
    const s = buildScale([S(0, 10_000)]);
    expect(s.toX(5_000)).toBeCloseTo(s.width / 2, 5);
  });

  it("returns an empty scale for no spans", () => {
    const s = buildScale([]);
    expect(s.width).toBe(0);
    expect(s.toX(12_345)).toBe(0);
  });
});

// ---------------------------------------------------------------------------

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

/** One user message, then an assistant turn that thinks and calls a tool. */
const SESSION: TranscriptItem[] = [
  msg("m1", "User", 1_000, { text: "do the thing" }),
  msg("m2", "Assistant", 5_000, {
    startedAtMs: 2_000,
    thinking: ["hmm"],
    toolCalls: [{ id: "t1", name: "Bash", endedAtMs: 12_000 }],
  }),
];

const MAIN = [agent({ id: "main", label: undefined })];

describe("buildTimeline", () => {
  it("draws one bar per entry on the main lane, in order", () => {
    const t = buildTimeline(SESSION, MAIN, [], 20_000);
    const main = t.lanes[0];
    expect(main.kind).toBe("main");
    expect(main.bars.map((b) => b.kind)).toEqual(["user", "thinking", "tool"]);
    const xs = main.bars.map((b) => b.x);
    expect([...xs].sort((a, b) => a - b)).toEqual(xs);
  });

  it("gives every bar something to scroll the transcript to", () => {
    const t = buildTimeline(SESSION, MAIN, [], 20_000);
    expect(t.lanes[0].bars.length).toBeGreaterThan(0);
    for (const bar of t.lanes[0].bars) expect(bar.entryId).toBeTruthy();
  });

  it("puts a tick at each turn start", () => {
    const t = buildTimeline(SESSION, MAIN, [], 20_000);
    expect(t.ticks).toHaveLength(1);
    expect(t.ticks[0].x).toBe(0);
  });

  it("drops a tick that would print on top of the one before it", () => {
    // Four quick turns rendered four labels at nearly the same pixel and read
    // as one line of garbled digits. Only a screenshot showed it.
    const rapid: TranscriptItem[] = [];
    for (let i = 0; i < 4; i++) {
      rapid.push(msg(`u${i}`, "User", 1_000 + i * 200, { text: `turn ${i}` }));
      rapid.push(
        msg(`a${i}`, "Assistant", 1_100 + i * 200, { startedAtMs: 1_050 + i * 200, text: "ok" }),
      );
    }
    const t = buildTimeline(rapid, MAIN, [], 20_000);
    expect(t.ticks.length).toBeGreaterThan(0);
    for (let i = 1; i < t.ticks.length; i++) {
      expect(t.ticks[i].x - t.ticks[i - 1].x).toBeGreaterThanOrEqual(56);
    }
  });

  it("labels a tick with a 24-hour time, so its width is fixed", () => {
    const t = buildTimeline(SESSION, MAIN, [], 20_000);
    expect(t.ticks[0].label).toMatch(/^\d{2}:\d{2}$/);
  });

  it("measures a still-running tool against now, not against zero", () => {
    const running: TranscriptItem[] = [
      msg("m1", "User", 1_000, { text: "go" }),
      msg("m2", "Assistant", 5_000, { startedAtMs: 2_000, toolCalls: [{ id: "t1", name: "Bash" }] }),
    ];
    const t = buildTimeline(running, MAIN, [], 20_000);
    const tool = t.lanes[0].bars.find((b) => b.kind === "tool");
    expect(tool?.live).toBe(true);
    expect(tool!.width).toBeGreaterThan(MIN_BAR_PX);
  });

  it("hangs a subagent under the main lane, anchored at its spawn", () => {
    const t = buildTimeline(
      SESSION,
      [...MAIN, agent({ id: "s1", label: "Explore", spawnedAtMs: 6_000, endedAtMs: 11_000 })],
      [],
      20_000,
    );
    const sub = t.lanes.find((l) => l.agentId === "s1");
    expect(sub?.kind).toBe("subagent");
    expect(sub?.span?.open).toBe(false);
    const tool = t.lanes[0].bars.find((b) => b.kind === "tool");
    expect(sub!.anchor!.x).toBeGreaterThanOrEqual(tool!.x);
    expect(sub!.anchor!.parentAgentId).toBe("main");
  });

  it("leaves a running subagent's span open", () => {
    const t = buildTimeline(
      SESSION,
      [...MAIN, agent({ id: "s1", spawnedAtMs: 6_000, endedAtMs: 0, status: "running" })],
      [],
      20_000,
    );
    expect(t.lanes.find((l) => l.agentId === "s1")?.span?.open).toBe(true);
  });

  it("leaves every fork's span open, because a conversation never ends", () => {
    const t = buildTimeline(SESSION, MAIN, [fork({ id: "f1", createdAtMs: 6_000 })], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "f1");
    expect(lane?.kind).toBe("fork");
    expect(lane?.span?.open).toBe(true);
  });

  it("nests a fork of a fork and anchors it to its parent", () => {
    const t = buildTimeline(
      SESSION,
      MAIN,
      [fork({ id: "f1", createdAtMs: 6_000 }), fork({ id: "f2", parent: "f1", createdAtMs: 8_000 })],
      20_000,
    );
    const child = t.lanes.find((l) => l.agentId === "f2");
    expect(child?.depth).toBe(1);
    expect(child?.anchor?.parentAgentId).toBe("f1");
  });

  it("shows a fork whose parent was deleted, at the top level", () => {
    const t = buildTimeline(SESSION, MAIN, [fork({ id: "f2", parent: "gone", createdAtMs: 8_000 })], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "f2");
    expect(lane).toBeDefined();
    expect(lane?.depth).toBe(0);
    expect(lane?.anchor?.parentAgentId).toBe("main");
  });

  it("shows a subagent whose parent was never delivered, at the top level", () => {
    const t = buildTimeline(
      SESSION,
      [...MAIN, agent({ id: "s9", parent: "missing", depth: 3, spawnedAtMs: 6_000, endedAtMs: 7_000 })],
      [],
      20_000,
    );
    const lane = t.lanes.find((l) => l.agentId === "s9");
    expect(lane?.anchor?.parentAgentId).toBe("main");
    expect(lane?.depth).toBe(0);
  });

  it("marks an agent with no usable stamps as unplaced rather than dropping it", () => {
    const t = buildTimeline(SESSION, [...MAIN, agent({ id: "old", label: "ancient" })], [], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "old");
    expect(lane?.placed).toBe(false);
    expect(lane?.span).toBeUndefined();
  });

  it("does not draw a subagent's report as a bar on its parent's lane", () => {
    // It already has a lane of its own; drawing it twice says the work happened
    // twice.
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
            {
              subagentId: "s1",
              label: "Explore",
              status: "completed",
              text: "done",
              spawnedAtMs: 6_000,
              endedAtMs: 11_000,
            },
          ],
          createdAtMs: 13_000,
        },
      },
    ];
    const t = buildTimeline(withResult, MAIN, [], 20_000);
    expect(t.lanes[0].bars.some((b) => b.entryId === "m3")).toBe(false);
  });

  it("ticks a compaction boundary and lets it be sought by seq", () => {
    const items: TranscriptItem[] = [
      ...SESSION,
      {
        kind: "compaction",
        value: {
          seq: 42,
          summary: "s",
          carriedState: "",
          covered: 3,
          tokensBefore: 100,
          tokensAfter: 10,
          manual: false,
          atMs: 13_000,
        },
      },
    ];
    const t = buildTimeline(items, MAIN, [], 20_000);
    const tick = t.lanes[0].bars.find((b) => b.kind === "compaction");
    expect(tick?.entryId).toBe("42");
  });

  it("survives a session with nothing in it", () => {
    const t = buildTimeline([], MAIN, [], 20_000);
    expect(t.width).toBe(0);
    expect(t.lanes[0].bars).toEqual([]);
  });
});
