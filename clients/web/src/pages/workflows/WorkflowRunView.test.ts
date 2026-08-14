import { describe, expect, it } from "vitest";
import { SessionStatusKind, type WorkflowRunGraph } from "../../api/types";
import {
  formatOutput,
  parkedStep,
  resumePoint,
  retryUnavailable,
} from "./WorkflowRunView";

/** A two-node run whose second node holds the execution at log index 1. */
function graph(over: Partial<WorkflowRunGraph> = {}): WorkflowRunGraph {
  const run = (index: number, agentId: string) => ({
    index,
    step: "s",
    agentId,
    attempt: 1,
    status: { type: "Running" as const, value: {} },
    output: undefined,
    error: undefined,
    startedAtMs: 0,
    endedAtMs: undefined,
    inputTokens: 0,
    outputTokens: 0,
  });
  return {
    workflow: "w",
    current: 1,
    start: "triage",
    nodes: [
      { step: "triage", runs: [run(0, "agent-0")] },
      { step: "fix", runs: [run(1, "agent-1")] },
    ],
    edges: [],
    output: undefined,
    error: undefined,
    inputTokens: 0,
    outputTokens: 0,
    ...over,
  } as WorkflowRunGraph;
}

describe("parkedStep", () => {
  /** `current` indexes the run log, not `nodes`, so the execution has to be
   * found across nodes — the whole reason this is worth its own function. */
  it("names the execution a parked run is waiting on", () => {
    expect(parkedStep(graph(), SessionStatusKind.AwaitingInput)).toEqual({
      step: "fix",
      agentId: "agent-1",
    });
  });

  it("is silent unless the run is actually waiting", () => {
    expect(parkedStep(graph(), SessionStatusKind.Running)).toBeUndefined();
    expect(parkedStep(graph(), SessionStatusKind.Finished)).toBeUndefined();
  });

  /** A session can report `AwaitingInput` with nothing in flight only if the
   * two reads raced; the banner must not claim a step then. */
  it("is silent when nothing is in flight", () => {
    expect(
      parkedStep(graph({ current: undefined }), SessionStatusKind.AwaitingInput),
    ).toBeUndefined();
  });
});

describe("resumePoint", () => {
  /** A run stopped part-way moves only by a retry, so the page has to name the
   * step to retry. Read off the log rather than a status word: `Suspended` was
   * a second vocabulary for what the newest execution already says. */
  it("names the interrupted step of a suspended run", () => {
    const g = graph();
    g.nodes[1]!.runs[0]!.status = { type: "Cancelled", value: {} };
    expect(resumePoint(g)).toEqual({ step: "fix", index: 1 });
  });

  /** A run can hold several cancelled attempts — a retry cancels the one it
   * supersedes — and only the newest is where it stopped. */
  it("takes the newest cancelled execution", () => {
    const g = graph();
    g.nodes[0]!.runs[0]!.status = { type: "Cancelled", value: {} };
    g.nodes[1]!.runs[0]!.status = { type: "Cancelled", value: {} };
    expect(resumePoint(g)?.index).toBe(1);
  });

  /** The case a status gate could not express: a retry appends rather than
   * truncating, so a later execution over a cancelled one means the run moved
   * on and there is nothing to resume. */
  it("is silent once a later execution has run", () => {
    const g = graph();
    g.nodes[0]!.runs[0]!.status = { type: "Cancelled", value: {} };
    g.nodes[1]!.runs[0]!.status = { type: "Concluded", value: {} };
    expect(resumePoint(g)).toBeUndefined();
  });

  it("is silent while the newest execution is still going", () => {
    expect(resumePoint(graph())).toBeUndefined();
  });
});

describe("retryUnavailable", () => {
  it("keeps all retries disabled while the run is active", () => {
    expect(retryUnavailable(SessionStatusKind.Running, false)).toBe(true);
  });

  it("covers a running attempt while the session document catches up", () => {
    expect(retryUnavailable(SessionStatusKind.Finished, false, graph().nodes[0]?.runs[0])).toBe(
      true,
    );
  });

  it("allows retries once the run has settled", () => {
    const finished = graph().nodes[0]?.runs[0];
    if (finished) finished.status = { type: "Concluded", value: {} };
    expect(retryUnavailable(SessionStatusKind.Finished, false, finished)).toBe(false);
    expect(retryUnavailable(SessionStatusKind.Finished, true, finished)).toBe(true);
  });
});

describe("formatOutput", () => {
  /** The same rule the server uses to hand one step's output to the next: a
   * string is its own answer rather than a quoted, escaped one. */
  it("passes a string through unquoted and renders anything else as JSON", () => {
    expect(formatOutput("all clear")).toBe("all clear");
    expect(formatOutput({ filed: 12 })).toBe('{\n  "filed": 12\n}');
  });
});
