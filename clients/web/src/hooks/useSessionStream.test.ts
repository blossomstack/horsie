import { describe, expect, it } from "vitest";
import { SessionStatusKind, type AgentLogEntry } from "../api/types";
import { fold } from "./useSessionStream";

/// The client owns a fold that must agree with the server's. That duplication
/// is the price of having one source instead of two, and these are what keep it
/// honest: each case is a fact the client used to be *told* out-of-band and now
/// computes, so a drift here is exactly the bug the redesign removed.

let seq = 0;
function lifecycle(kind: string, value: unknown): AgentLogEntry {
  return {
    seq: seq++,
    atMs: 1_700_000_000_000 + seq,
    body: { type: "Lifecycle", value: { kind, value } },
  } as unknown as AgentLogEntry;
}

function reset() {
  seq = 0;
}

describe("fold", () => {
  it("is empty before anything has happened", () => {
    reset();
    const f = fold([]);
    expect(f.status).toBeNull();
    expect(f.queued).toEqual([]);
    expect(f.error).toBeNull();
  });

  it("derives the queue from MessageQueued and TurnBegan", () => {
    reset();
    const f = fold([
      lifecycle("MessageQueued", { id: "m1", text: "one" }),
      lifecycle("MessageQueued", { id: "m2", text: "two" }),
      lifecycle("TurnBegan", { consumed: ["m1"], answered: [] }),
    ]);
    expect(f.queued).toEqual([{ id: "m2", text: "two" }]);
    expect(f.status).toBe(SessionStatusKind.Running);
  });

  // The #246 bug class, now unrepresentable: the queue and the turn that drains
  // it are the same log, so there is no ordering between them to get wrong.
  it("drains a message accepted and consumed in one turn", () => {
    reset();
    const f = fold([
      lifecycle("MessageQueued", { id: "m1", text: "one" }),
      lifecycle("TurnBegan", { consumed: ["m1"], answered: [] }),
      lifecycle("TurnEnded", { outcome: { kind: "Ended", value: {} } }),
    ]);
    expect(f.queued).toEqual([]);
    expect(f.status).toBe(SessionStatusKind.Idle);
  });

  it("reports a failed turn and its reason", () => {
    reset();
    const f = fold([
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
      lifecycle("TurnEnded", {
        outcome: { kind: "Failed", value: { error: "boom" } },
      }),
    ]);
    expect(f.status).toBe(SessionStatusKind.Failed);
    expect(f.error).toBe("boom");
  });

  // What `errorLive` and its Resync release existed for. A turn that starts
  // supersedes the last one's failure, and here that is simply the later entry
  // winning rather than a latch someone has to remember to clear.
  it("clears a previous failure when the next turn starts", () => {
    reset();
    const f = fold([
      lifecycle("TurnEnded", {
        outcome: { kind: "Failed", value: { error: "boom" } },
      }),
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
    ]);
    expect(f.error).toBeNull();
    expect(f.status).toBe(SessionStatusKind.Running);
  });

  it("parks on an ask and releases it when the turn answers", () => {
    reset();
    const f = fold([
      lifecycle("AskRecorded", { toolCallId: "tc1", question: "which?" }),
    ]);
    expect(f.status).toBe(SessionStatusKind.AwaitingInput);
    expect(f.pendingAsks).toEqual([{ toolCallId: "tc1", question: "which?" }]);

    reset();
    const answered = fold([
      lifecycle("AskRecorded", { toolCallId: "tc1", question: "which?" }),
      lifecycle("TurnBegan", { consumed: [], answered: ["tc1"] }),
    ]);
    expect(answered.pendingAsks).toEqual([]);
  });

  // A plain message during a park *abandons* the questions rather than
  // answering them, so `answered` is empty — and the park is over all the same.
  // Filtering the pending set by `answered` left the answer cards on screen
  // with nothing behind them to answer.
  it("ends the park when a turn abandons the questions", () => {
    reset();
    const f = fold([
      lifecycle("AskRecorded", { toolCallId: "tc1", question: "which?" }),
      lifecycle("MessageQueued", { id: "m1", text: "never mind" }),
      lifecycle("TurnBegan", { consumed: ["m1"], answered: [] }),
    ]);
    expect(f.pendingAsks).toEqual([]);
    expect(f.status).toBe(SessionStatusKind.Running);
    expect(f.queued).toEqual([]);
  });

  it("takes the last task list and remembers which entry it came from", () => {
    reset();
    const f = fold([
      lifecycle("TaskList", { tasks: [{ id: 1, content: "a", status: "Pending" }] }),
      lifecycle("TaskList", { tasks: [{ id: 1, content: "a", status: "Completed" }] }),
    ]);
    expect(f.tasks).toHaveLength(1);
    expect(f.tasks?.[0].status).toBe("Completed");
    // The seq is what makes an agent-document read comparable against this
    // rather than a guess about which is fresher.
    expect(f.tasksSeq).toBe(1);
  });

  it("shows preparation progress and drops it once the turn ends", () => {
    reset();
    const running = fold([
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
      lifecycle("Preparing", { stage: "scanning_workspace", detail: null }),
    ]);
    expect(running.progression).toEqual({
      stage: "scanning_workspace",
      detail: null,
    });

    reset();
    const done = fold([
      lifecycle("Preparing", { stage: "scanning_workspace", detail: null }),
      lifecycle("TurnEnded", { outcome: { kind: "Ended", value: {} } }),
    ]);
    expect(done.progression).toBeNull();
  });

  // A turn's setup and the session's sandbox both used to arrive as
  // `Provisioning` with a `stage` string, and both used the label "ready".
  // They are separate variants now, and only the sandbox moves the status.
  it("separates the session's runtime from a turn's preparation", () => {
    reset();
    const prep = fold([
      lifecycle("Preparing", { stage: "ready", detail: null }),
    ]);
    expect(prep.status).toBeNull();

    reset();
    const acquiring = fold([
      lifecycle("Runtime", { status: { kind: "Acquiring", value: {} }, detail: null }),
    ]);
    expect(acquiring.status).toBe(SessionStatusKind.Provisioning);

    reset();
    const failed = fold([
      lifecycle("Runtime", {
        status: { kind: "Failed", value: {} },
        detail: "vendor offline",
      }),
    ]);
    expect(failed.status).toBe(SessionStatusKind.Failed);
    expect(failed.reason).toBe("vendor offline");
  });

  // What the vendor said, carried all the way through. The server has always
  // had these words — "the machine is booting" — and used to drop them at the
  // runtime manager, so a provisioning session reported a stage and nothing
  // else for as long as the machine took to come up.
  it("carries the vendor's own words through both sources of a wait", () => {
    reset();
    const provisioning = fold([
      lifecycle("Runtime", {
        status: { kind: "Acquiring", value: {} },
        detail: "the machine is booting",
      }),
    ]);
    expect(provisioning.progression).toEqual({
      stage: "runtime_acquiring",
      detail: "the machine is booting",
    });

    reset();
    const acquiring = fold([
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
      lifecycle("Preparing", {
        stage: "acquiring_runtime",
        detail: "the machine is resuming",
      }),
    ]);
    expect(acquiring.progression).toEqual({
      stage: "acquiring_runtime",
      detail: "the machine is resuming",
    });
  });

  // The server used to send these facts twice: once as a typed entry nothing
  // read, and once as a free-text `Provisioning` entry built for display. It now
  // sends only the typed one, so the progress line is derived here.
  it("shows a subagent's progress from its own entry", () => {
    reset();
    const f = fold([
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
      lifecycle("SubAgent", { id: "abc", label: "audit", status: "running" }),
    ]);
    expect(f.progression).toEqual({
      stage: "subagent_running",
      detail: '"audit" (abc)',
    });
  });

  it("shows a workflow step's progress from its own entry", () => {
    reset();
    const f = fold([
      lifecycle("TurnBegan", { consumed: [], answered: [] }),
      lifecycle("Step", { index: 0, name: "review", status: "started" }),
    ]);
    // The name, not the index: an index identifies the execution, the name is
    // what a person reading the run recognises.
    expect(f.progression).toEqual({ stage: "step_started", detail: "review" });
  });

  it("drops a subagent's progress once the turn ends", () => {
    reset();
    const f = fold([
      lifecycle("SubAgent", { id: "abc", label: "audit", status: "completed" }),
      lifecycle("TurnEnded", { outcome: { kind: "Ended", value: {} } }),
    ]);
    expect(f.progression).toBeNull();
  });

  it("treats a terminal session failure as unrecoverable", () => {
    reset();
    const f = fold([lifecycle("SessionFailed", { reason: "vendor refused" })]);
    expect(f.status).toBe(SessionStatusKind.Unrecoverable);
    expect(f.reason).toBe("vendor refused");
  });

  it("ignores entries that are not lifecycle", () => {
    reset();
    const f = fold([
      {
        seq: 0,
        atMs: 1,
        body: {
          type: "Llm",
          value: { id: "m1", role: "User", parts: [], createdAtMs: 1 },
        },
      } as unknown as AgentLogEntry,
    ]);
    expect(f.status).toBeNull();
  });
});
