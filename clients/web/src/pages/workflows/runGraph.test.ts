import { describe, expect, it } from "vitest";
import { SessionStatusKind, type WorkflowRunGraph } from "../../api/types";
import { formatOutput, retryUnavailable } from "./runGraph";

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
