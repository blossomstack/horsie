import { describe, expect, it } from "vitest";
import { buildSegments } from "./transcriptSegments";
import { groupTurns } from "../components/Transcript";
import type {
  RenderedMessage,
  RenderedSubAgent,
  RenderedToolCall,
  TranscriptItem,
} from "../hooks/useSessionStream";

function tool(id: string, endedAtMs?: number): RenderedToolCall {
  return { id, name: "read_file", input: {}, running: false, endedAtMs, hooks: [] };
}

function assistant(m: Partial<RenderedMessage> & { id: string }): RenderedMessage {
  return {
    role: "Assistant",
    text: "",
    thinking: [],
    toolCalls: [],
    subagentResults: [],
    ...m,
  };
}

const workSpans = (segments: ReturnType<typeof buildSegments>) =>
  segments
    .filter((s) => s.kind === "work")
    .map((s) => [s.startedAtMs, s.endedAtMs]);

describe("buildSegments work spans", () => {
  it("spans from the provider call to the last tool that answered", () => {
    const segments = buildSegments([
      assistant({
        id: "m1",
        thinking: ["hmm"],
        toolCalls: [tool("t1", 5_000), tool("t2", 9_000)],
        startedAtMs: 1_000,
        createdAtMs: 3_000,
      }),
    ]);
    expect(workSpans(segments)).toEqual([[1_000, 9_000]]);
  });

  it("merges consecutive messages into one span", () => {
    const segments = buildSegments([
      assistant({
        id: "m1",
        toolCalls: [tool("t1", 4_000)],
        startedAtMs: 1_000,
        createdAtMs: 2_000,
      }),
      assistant({
        id: "m2",
        toolCalls: [tool("t2", 12_000)],
        startedAtMs: 5_000,
        createdAtMs: 6_000,
      }),
    ]);
    expect(workSpans(segments)).toEqual([[1_000, 12_000]]);
  });

  it("starts a fresh span after text breaks the group", () => {
    const segments = buildSegments([
      assistant({
        id: "m1",
        toolCalls: [tool("t1", 4_000)],
        startedAtMs: 1_000,
        createdAtMs: 2_000,
      }),
      assistant({ id: "m2", text: "here you go", startedAtMs: 5_000, createdAtMs: 6_000 }),
      assistant({
        id: "m3",
        toolCalls: [tool("t2", 30_000)],
        startedAtMs: 20_000,
        createdAtMs: 21_000,
      }),
    ]);
    expect(workSpans(segments)).toEqual([
      [1_000, 4_000],
      [20_000, 30_000],
    ]);
  });

  it("falls back to the message stamp when no provider start was recorded", () => {
    // Only assistant messages carry `startedAtMs`; anything else that
    // contributes work is bounded by its own stamp rather than dropped.
    const segments = buildSegments([
      assistant({ id: "m1", toolCalls: [tool("t1", 8_000)], createdAtMs: 2_000 }),
    ]);
    expect(workSpans(segments)).toEqual([[2_000, 8_000]]);
  });

  it("leaves a live group's span unknown", () => {
    const segments = buildSegments([], {
      text: "",
      orphanTools: [tool("t1")],
    });
    expect(workSpans(segments)).toEqual([[undefined, undefined]]);
  });
});

function sub(label: string, over: Partial<RenderedSubAgent> = {}): RenderedSubAgent {
  return {
    subagentId: `id-${label}`,
    label,
    status: "completed",
    text: "three stale crates",
    spawnedAtMs: 100,
    endedAtMs: 400,
    ...over,
  };
}

function user(m: Partial<RenderedMessage> & { id: string }): RenderedMessage {
  return {
    role: "User",
    text: "",
    thinking: [],
    toolCalls: [],
    subagentResults: [],
    ...m,
  };
}

/** `groupTurns` reads transcript items, of which a message is one kind. */
const item = (m: RenderedMessage): TranscriptItem => ({
  kind: "message",
  value: m,
});

describe("subagent results in a turn", () => {
  /** The point of the whole change: a delegating session must not read as if
   *  the person kept pasting reports to themselves. */
  it("attaches an owed-only result to the preceding assistant entry", () => {
    const turns = groupTurns([
      item(assistant({ id: "a1", text: "delegating" })),
      item(user({ id: "u1", subagentResults: [sub("audit")] })),
    ]);
    expect(turns).toHaveLength(1);
    expect(turns[0].kind).toBe("assistant");
  });

  it("puts results above the bubble when a turn carries both", () => {
    const turns = groupTurns([
      item(assistant({ id: "a1", text: "delegating" })),
      item(
        user({
          id: "u1",
          text: "check the lockfile too",
          subagentResults: [sub("audit")],
        }),
      ),
    ]);
    expect(turns.map((t) => t.kind)).toEqual(["assistant", "user"]);
    const first = turns[0];
    if (first.kind !== "assistant") throw new Error("expected an assistant turn");
    // The synthetic message carries the results and nothing else: with text on
    // it, `buildSegments` would emit the user's words into the agent's thread.
    expect(first.msgs.at(-1)?.subagentResults).toHaveLength(1);
    expect(first.msgs.at(-1)?.text).toBe("");
  });

  it("opens an assistant entry when a result has nothing to attach to", () => {
    const turns = groupTurns([
      item(user({ id: "u1", subagentResults: [sub("audit")] })),
    ]);
    expect(turns).toHaveLength(1);
    expect(turns[0].kind).toBe("assistant");
  });

  it("renders a result as a work item carrying its own span", () => {
    const segments = buildSegments([
      user({ id: "u1", subagentResults: [sub("audit")] }),
    ]);
    expect(segments).toHaveLength(1);
    const work = segments[0];
    if (work.kind !== "work") throw new Error("expected a work segment");
    expect(work.items).toEqual([{ kind: "subagent", result: sub("audit") }]);
    expect([work.startedAtMs, work.endedAtMs]).toEqual([100, 400]);
  });

  /** A subagent journaled before spans were recorded must not drag the group's
   *  duration back to the epoch. */
  it("ignores a result with no recorded span", () => {
    const segments = buildSegments([
      user({
        id: "u1",
        subagentResults: [sub("audit", { spawnedAtMs: 0, endedAtMs: 0 })],
      }),
    ]);
    const work = segments[0];
    if (work.kind !== "work") throw new Error("expected a work segment");
    expect([work.startedAtMs, work.endedAtMs]).toEqual([undefined, undefined]);
  });
});
