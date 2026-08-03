import { describe, expect, it } from "vitest";
import { buildSegments } from "./transcriptSegments";
import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";

function tool(id: string, endedAtMs?: number): RenderedToolCall {
  return { id, name: "read_file", input: {}, running: false, endedAtMs };
}

function assistant(m: Partial<RenderedMessage> & { id: string }): RenderedMessage {
  return {
    role: "Assistant",
    text: "",
    thinking: [],
    toolCalls: [],
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
