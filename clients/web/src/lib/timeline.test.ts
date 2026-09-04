import { describe, expect, it } from "vitest";
import type { SubSessionView, SubAgentView } from "../api/types";
import type { TranscriptItem } from "../hooks/useSessionStream";
import { runNodeId } from "./agentTree";
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
      artifacts: [],
    })),
    subagentResults: [],
    artifacts: [],
    createdAtMs,
    startedAtMs: extra.startedAtMs,
  },
});

const agent = (o: Partial<SubAgentView> & { id: string }): SubAgentView => ({
  title: o.id,
  kind: "subagent",
  stats: {
      usage: { inputTokens: 0, outputTokens: 0 },
      subtreeUsage: { inputTokens: 0, outputTokens: 0 },
      contextTokens: 0,
      efficiency: {
              providerCalls: 0,
        providerGenerationMs: 0,
        maxProviderGenerationMs: 0,
              toolCalls: 0,
        toolExecutionMs: 0,
        maxToolExecutionMs: 0,
              failedToolCalls: 0,
              toolResultBytes: 0,
        originalToolResultBytes: 0,
        truncatedToolResultBytes: 0,
        spilledToolResultBytes: 0,
              completedRuns: 0,
              abortedRuns: 0,
              compactions: 0,
            },
    },
  depth: 0,
  status: "completed",
  spawnedAtMs: 0,
  endedAtMs: 0,
  ...o,
});

const subSession = (o: Partial<SubSessionView> & { id: string }): SubSessionView => ({
  title: o.id,
  status: "idle",
  createdAtMs: 0,
  lastActivityMs: 0,
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

const MAIN = [agent({ id: "main", kind: "main", title: undefined })];

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

  it("never repeats a tick label", () => {
    // Several turns inside one minute drew `18:27 18:27 18:27` — three marks
    // that say nothing. Spaced far enough apart to clear the collision rule,
    // so only the duplicate-label rule can catch them.
    const spaced: TranscriptItem[] = [];
    for (let i = 0; i < 3; i++) {
      spaced.push(msg(`u${i}`, "User", 1_000 + i * 15_000, { text: `turn ${i}` }));
      spaced.push(
        msg(`a${i}`, "Assistant", 9_000 + i * 15_000, {
          startedAtMs: 2_000 + i * 15_000,
          text: "ok",
        }),
      );
    }
    const t = buildTimeline(spaced, MAIN, [], 60_000);
    expect(new Set(t.ticks.map((k) => k.label)).size).toBe(t.ticks.length);
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
      [...MAIN, agent({ id: "s1", title: "Explore", spawnedAtMs: 6_000, endedAtMs: 11_000 })],
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

  it("leaves every sub session's span open, because a session never ends", () => {
    const t = buildTimeline(SESSION, MAIN, [subSession({ id: "f1", createdAtMs: 6_000 })], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "f1");
    expect(lane?.kind).toBe("sub_session");
    expect(lane?.span?.open).toBe(true);
  });

  it("nests a subSession of a subSession and anchors it to its parent", () => {
    const t = buildTimeline(
      SESSION,
      MAIN,
      [subSession({ id: "f1", createdAtMs: 6_000 }), subSession({ id: "f2", parent: "f1", createdAtMs: 8_000 })],
      20_000,
    );
    const child = t.lanes.find((l) => l.agentId === "f2");
    // Depth 2: one edge from the root to `f1`, another from `f1` to this.
    expect(child?.depth).toBe(2);
    expect(child?.anchor?.parentAgentId).toBe("f1");
  });

  it("shows a sub session whose parent was deleted, at the top level", () => {
    const t = buildTimeline(SESSION, MAIN, [subSession({ id: "f2", parent: "gone", createdAtMs: 8_000 })], 20_000);
    const lane = t.lanes.find((l) => l.agentId === "f2");
    expect(lane).toBeDefined();
    // Top level *relative to the root*, which is depth 1.
    expect(lane?.depth).toBe(1);
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
    expect(lane?.depth).toBe(1);
  });

  it("marks an agent with no usable stamps as unplaced rather than dropping it", () => {
    const t = buildTimeline(SESSION, [...MAIN, agent({ id: "old", title: "ancient" })], [], 20_000);
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
              title: "Explore",
              status: "completed",
              text: "done",
              spawnedAtMs: 6_000,
              endedAtMs: 11_000,
            },
          ],
          artifacts: [],
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

describe("parallel tool calls", () => {
  it("lays two calls issued together end to end rather than on top of each other", () => {
    // Both were issued at the assistant message's end, so at their true starts
    // they drew at the same x and the shorter hid under the longer.
    const parallel: TranscriptItem[] = [
      msg("m1", "User", 1_000, { text: "go" }),
      msg("m2", "Assistant", 2_000, {
        startedAtMs: 1_500,
        toolCalls: [
          { id: "t1", name: "bash", endedAtMs: 6_000 },
          { id: "t2", name: "grep", endedAtMs: 4_000 },
        ],
      }),
    ];
    const bars = buildTimeline(parallel, MAIN, [], 10_000).lanes[0].bars.filter(
      (b) => b.kind === "tool",
    );
    expect(bars).toHaveLength(2);
    // Finish order, and no overlap.
    expect(bars[0].title).toBe("grep");
    expect(bars[1].title).toBe("bash");
    expect(bars[1].x).toBeGreaterThanOrEqual(bars[0].x + bars[0].width - 1);
    // Each keeps its own true duration.
    expect(bars[0].detail).toBe("2.0s");
    expect(bars[1].detail).toBe("4.0s");
  });

  it("gives every bar its own key, so no two share a test id", () => {
    const parallel: TranscriptItem[] = [
      msg("m1", "User", 1_000, { text: "go" }),
      msg("m2", "Assistant", 2_000, {
        startedAtMs: 1_500,
        thinking: ["hmm"],
        text: "done",
        toolCalls: [
          { id: "t1", name: "bash", endedAtMs: 6_000 },
          { id: "t2", name: "grep", endedAtMs: 4_000 },
        ],
      }),
    ];
    const bars = buildTimeline(parallel, MAIN, [], 10_000).lanes[0].bars;
    expect(new Set(bars.map((b) => b.key)).size).toBe(bars.length);
  });
});

describe("folding", () => {
  /** Folding a lane removes the lanes hanging off it.
   *
   * It used to be the renderer's, done by skipping lanes deeper than a folded
   * one — and it silently did nothing, because everything hanging off the root
   * arrived at the root's own depth. Nothing was ever *deeper* than the lane it
   * was under, so nothing was ever skipped and the chevron was inert. */
  it("removes the lanes hanging off a folded one, at any depth", () => {
    const roster = [
      agent({ id: "main", kind: "main", title: undefined }),
      agent({ id: "p", title: "audit", spawnedAtMs: 5_500, endedAtMs: 9_000 }),
      agent({ id: "c", title: "lockfile", parent: "p", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
      agent({ id: "other", title: "sibling", spawnedAtMs: 5_600, endedAtMs: 9_000 }),
    ];
    const open = buildTimeline(SESSION, roster, [], 20_000);
    expect(open.lanes.map((l) => l.agentId)).toContain("c");

    const folded = buildTimeline(SESSION, roster, [], 20_000, {}, undefined, ["p"]);
    const ids = folded.lanes.map((l) => l.agentId);
    expect(ids).toContain("p");
    expect(ids).not.toContain("c");
    // A sibling is not a child, so it stays.
    expect(ids).toContain("other");
  });

  /** Folding the root must not take its own chevron with it.
   *
   * `hasChildren` was read off the lanes that survived the fold, so a folded
   * root reported no children, so the renderer drew it without the control
   * that unfolds it: collapsing the whole session was a one-way door, and the
   * only way back was a reload. Every other lane was fine — a member's child
   * count is counted off the roster, which a fold cannot change. */
  it("keeps the root's disclosure control while the root is folded", () => {
    const roster = [
      agent({ id: "main", kind: "main", title: undefined }),
      agent({ id: "p", title: "audit", spawnedAtMs: 5_500, endedAtMs: 9_000 }),
    ];
    const folded = buildTimeline(SESSION, roster, [], 20_000, {}, undefined, ["main"]);
    expect(folded.lanes.map((l) => l.agentId)).toEqual(["main"]);
    expect(folded.lanes[0].hasChildren).toBe(true);
  });

  it("says the root has nothing to disclose when it really has nothing", () => {
    const folded = buildTimeline(SESSION, MAIN, [], 20_000, {}, undefined, ["main"]);
    expect(folded.lanes[0].hasChildren).toBe(false);
  });

  /** Subagents first, then the sessions branched off — under every agent, at
   *  every depth. The timeline used to say it with a labelled rule drawn once,
   *  at the first sub session in a flat list of lanes; with two agents that
   *  each had both, that rule landed inside one of them. */
  it("lays the delegated work above the sessions branched from the same agent", () => {
    const roster = [
      agent({ id: "main", kind: "main", title: undefined }),
      agent({ id: "a1", title: "audit", spawnedAtMs: 8_000, endedAtMs: 9_000 }),
    ];
    const t = buildTimeline(
      SESSION,
      roster,
      [subSession({ id: "f1", title: "branch", createdAtMs: 6_000, lastActivityMs: 7_000 })],
      20_000,
    );
    // The sub session was branched first and is still drawn second.
    expect(t.lanes.map((l) => l.agentId)).toEqual(["main", "a1", "f1"]);
  });
});

/** A workflow run is a session with no main agent: it *is* its steps.
 *
 * Rooted on whichever step's page you were on, the others were orphans rescued
 * onto it — and worse, the axis was built from that one step's transcript, so
 * `toX` clamped every step that ran before or after it to an edge. On the
 * first step's page the two that followed drew as slivers *inside* it. */
describe("workflow runs", () => {
  const step = (id: string, at: number, took: number, status = "completed"): SubAgentView =>
    agent({
      id,
      kind: "step",
      title: id,
      status,
      spawnedAtMs: at,
      endedAtMs: at + took,
      run: "run-1",
      workflow: "nightly-audit",
    });
  const RUN_ROOT = runNodeId("run-1");

  const RUN = [step("gather", 1_000, 4_000), step("review", 5_000, 2_000), step("report", 7_000, 1_000)];

  it("roots on the run, with every step hanging off it", () => {
    const t = buildTimeline([], RUN, [], 20_000, {}, "gather", [], "nightly-audit");
    expect(t.lanes.map((l) => [l.agentId, l.kind, l.depth])).toEqual([
      [RUN_ROOT, "run", 0],
      ["gather", "step", 1],
      ["review", "step", 1],
      ["report", "step", 1],
    ]);
    expect(t.lanes[0].label).toBe("nightly-audit");
    // Nothing of its own: what a run did is what its steps did.
    expect(t.lanes[0].bars).toEqual([]);
    expect(t.lanes[0].hasChildren).toBe(true);
  });

  it("lays the axis over the whole run, not over one step's transcript", () => {
    const t = buildTimeline([], RUN, [], 20_000, {}, "gather", [], "nightly-audit");
    const [, gather, review, report] = t.lanes;
    // Each step starts where the last one ended, left to right, and none of
    // them is clamped onto another.
    expect(gather.span?.x).toBe(0);
    expect(review.span?.x).toBeGreaterThan((gather.span?.x ?? 0) + (gather.span?.width ?? 0) - 1);
    expect(report.span?.x).toBeGreaterThan(review.span?.x ?? 0);
    // The longest step is the widest, which is the scale doing its job.
    expect(gather.span?.width).toBeGreaterThan(review.span?.width ?? 0);
  });

  it("draws the step being read on its own lane, from the history it is handed", () => {
    const own: TranscriptItem[] = [
      msg("s1", "User", 1_000, { text: "gather it" }),
      msg("s2", "Assistant", 3_000, { startedAtMs: 1_500, text: "gathered" }),
    ];
    const t = buildTimeline([], RUN, [], 20_000, { gather: own }, "gather", [], "nightly-audit");
    const gather = t.lanes.find((l) => l.agentId === "gather");
    expect(gather?.bars.length).toBe(2);
    // On the run's axis, so a step's own work lines up inside its own span.
    const first = gather?.bars[0];
    expect(first?.x).toBeGreaterThanOrEqual(gather?.span?.x ?? 0);
  });

  it("marks a tick per step rather than per turn of whichever step this is", () => {
    const t = buildTimeline([], RUN, [], 20_000, {}, "gather", [], "nightly-audit");
    expect(t.ticks.length).toBeGreaterThan(0);
    expect(t.ticks[0].x).toBe(0);
  });

  /* An ordinary session whose agent invoked a workflow is not a run: it keeps
     its own root and its axis, and the run is a lane under it with its
     executions beneath — foldable, like any other. */
  it("gives a run an agent invoked a lane of its own under that session", () => {
    const roster = [agent({ id: "main", kind: "main", title: undefined }), ...RUN];
    const t = buildTimeline(SESSION, roster, [], 20_000, {}, undefined, []);
    expect(t.lanes[0]).toMatchObject({ agentId: "main", kind: "main" });
    expect(t.lanes.map((l) => [l.agentId, l.depth])).toEqual([
      ["main", 0],
      [RUN_ROOT, 1],
      ["gather", 2],
      ["review", 2],
      ["report", 2],
    ]);
  });

  it("keeps the run's own chevron while the run is folded", () => {
    const t = buildTimeline([], RUN, [], 20_000, {}, "gather", [RUN_ROOT], "nightly-audit");
    expect(t.lanes.map((l) => l.agentId)).toEqual([RUN_ROOT]);
    expect(t.lanes[0].hasChildren).toBe(true);
  });
});

describe("scope", () => {
  /** A page scoped to one agent draws that agent's work. The transcript above
   *  it is that agent's, and a lane labelled for the main agent over a
   *  subagent's bars was the picture contradicting the prose beside it. */
  it("roots the timeline on the agent it is given", () => {
    const t = buildTimeline(
      SESSION,
      [
        agent({ id: "main", kind: "main", title: "the session" }),
        agent({ id: "p", title: "audit", spawnedAtMs: 5_500, endedAtMs: 9_000 }),
        agent({ id: "c", title: "lockfile", parent: "p", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
      ],
      [],
      20_000,
      {},
      "p",
    );
    expect(t.lanes[0].agentId).toBe("p");
    expect(t.lanes[0].label).toBe("audit");
    expect(t.lanes[0].kind).toBe("subagent");
    // Only what hangs off it: the main agent is above this run, not below it.
    expect(t.lanes.map((l) => l.agentId)).toEqual(["p", "c"]);
  });
});

describe("nesting", () => {
  it("puts a subagent's child under it, indented, not beside it", () => {
    // The roster is keyed by uuid, so a child can sort above its parent and the
    // two read as siblings. Only a screenshot showed it.
    const t = buildTimeline(
      SESSION,
      [
        agent({ id: "main", title: undefined }),
        agent({ id: "zzz-child", title: "lockfile", parent: "aaa-parent", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
        agent({ id: "aaa-parent", title: "audit", spawnedAtMs: 5_500, endedAtMs: 9_000 }),
      ],
      [],
      20_000,
    );
    const order = t.lanes.filter((l) => l.kind === "subagent").map((l) => [l.label, l.depth]);
    // Depth 1 for anything hanging off the root, not 0. A child drawn at its
    // parent's own depth is a child no fold can hide: the fold walk reads
    // depth, and nothing was ever deeper than the lane it was under.
    expect(order).toEqual([
      ["audit", 1],
      ["lockfile", 2],
    ]);
  });

  it("anchors a nested subagent to its parent, not to the main agent", () => {
    const t = buildTimeline(
      SESSION,
      [
        agent({ id: "main", title: undefined }),
        agent({ id: "p", title: "audit", spawnedAtMs: 5_500, endedAtMs: 9_000 }),
        agent({ id: "c", title: "lockfile", parent: "p", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
      ],
      [],
      20_000,
    );
    expect(t.lanes.find((l) => l.agentId === "c")?.anchor?.parentAgentId).toBe("p");
  });

  it("appends a subagent whose parent chain is a cycle rather than hanging", () => {
    const t = buildTimeline(
      SESSION,
      [
        agent({ id: "main", title: undefined }),
        agent({ id: "a", parent: "b", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
        agent({ id: "b", parent: "a", spawnedAtMs: 6_000, endedAtMs: 7_000 }),
      ],
      [],
      20_000,
    );
    expect(t.lanes.filter((l) => l.kind === "subagent")).toHaveLength(2);
  });

  it("ends a subSession's lane at its last activity, not at the edge of the session", () => {
    const t = buildTimeline(
      SESSION,
      MAIN,
      [subSession({ id: "f1", createdAtMs: 6_000, lastActivityMs: 8_000, status: "idle" })],
      20_000,
    );
    const lane = t.lanes.find((l) => l.agentId === "f1");
    expect(lane?.span?.open).toBe(false);
    expect(lane!.span!.width).toBeLessThan(t.width);
  });

  it("leaves a subSession that is still running open", () => {
    const t = buildTimeline(
      SESSION,
      MAIN,
      [subSession({ id: "f1", createdAtMs: 6_000, lastActivityMs: 8_000, status: "running" })],
      20_000,
    );
    expect(t.lanes.find((l) => l.agentId === "f1")?.span?.open).toBe(true);
  });

  it("draws an expanded agent's own history on its lane, on the session's scale", () => {
    const own: TranscriptItem[] = [
      msg("s1", "Assistant", 7_000, { startedAtMs: 6_200, toolCalls: [{ id: "x", name: "grep", endedAtMs: 8_000 }] }),
    ];
    const t = buildTimeline(
      SESSION,
      [...MAIN, agent({ id: "sub", title: "audit", spawnedAtMs: 6_000, endedAtMs: 9_000 })],
      [],
      20_000,
      { sub: own },
    );
    const lane = t.lanes.find((l) => l.agentId === "sub");
    expect(lane!.bars.length).toBeGreaterThan(0);
    // On the session's scale: its bars sit inside its own span.
    for (const b of lane!.bars) expect(b.x).toBeGreaterThanOrEqual(lane!.span!.x - 1);
  });
});

describe("an expanded subSession", () => {
  it("shows only what it did, not the parent history it was seeded with", () => {
    // `/fork` copies the source's log, timestamps and all. Drawn unfiltered a
    // sub session claimed to have been working through turns that predate it.
    const copied: TranscriptItem[] = [
      msg("old", "Assistant", 3_000, { startedAtMs: 2_000, text: "from the parent" }),
      msg("mine", "Assistant", 9_000, { startedAtMs: 8_000, text: "my own work" }),
    ];
    const t = buildTimeline(
      SESSION,
      MAIN,
      [subSession({ id: "f1", createdAtMs: 7_000, lastActivityMs: 9_000 })],
      20_000,
      { f1: copied },
    );
    const bars = t.lanes.find((l) => l.agentId === "f1")!.bars;
    expect(bars.map((b) => b.entryId)).toEqual(["mine"]);
  });
});
