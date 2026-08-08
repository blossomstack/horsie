import { describe, expect, it } from "vitest";
import type { WorkflowRunGraph } from "../../api/types";
import { formatOutput, parkedStep, resumePoint } from "./WorkflowRunView";

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
    status: { type: "AwaitingInput", value: {} },
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
    expect(parkedStep(graph())).toEqual({ step: "fix", agentId: "agent-1" });
  });

  it("is silent unless the run is actually waiting", () => {
    expect(
      parkedStep(graph({ status: { type: "Running", value: {} } })),
    ).toBeUndefined();
    expect(
      parkedStep(graph({ status: { type: "Finished", value: {} } })),
    ).toBeUndefined();
  });

  /** A run can report `AwaitingInput` with nothing in flight only if the two
   * reads raced; the banner must not claim a step then. */
  it("is silent when nothing is in flight", () => {
    expect(parkedStep(graph({ current: undefined }))).toBeUndefined();
  });
});

describe("resumePoint", () => {
  /** A suspended run moves only by a retry, so the page has to name the step to
   * retry. This state became reachable at all once interruption stopped leaving
   * runs wedged as `Running`. */
  it("names the interrupted step of a suspended run", () => {
    const g = graph({ status: { type: "Suspended", value: {} } });
    g.nodes[1]!.runs[0]!.status = { type: "Cancelled", value: {} };
    expect(resumePoint(g)).toEqual({ step: "fix", index: 1 });
  });

  /** A run can hold several cancelled attempts — a retry cancels the one it
   * supersedes — and only the newest is where it stopped. */
  it("takes the newest cancelled execution", () => {
    const g = graph({ status: { type: "Suspended", value: {} } });
    g.nodes[0]!.runs[0]!.status = { type: "Cancelled", value: {} };
    g.nodes[1]!.runs[0]!.status = { type: "Cancelled", value: {} };
    expect(resumePoint(g)?.index).toBe(1);
  });

  it("is silent for every other status", () => {
    for (const type of ["Running", "AwaitingInput", "Finished", "Failed"] as const) {
      expect(resumePoint(graph({ status: { type, value: {} } }))).toBeUndefined();
    }
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
