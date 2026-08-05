import { describe, expect, it } from "vitest";
import type { HookRecord } from "../api/types";
import { hookSummary, systemMessage, toolScope } from "./hookSummary";

function rec(action: HookRecord["action"]): HookRecord {
  return { plugin: "guard", durationMs: 4, action };
}

const CALL = { tool: "bash", toolCallId: "tc1" };

describe("toolScope", () => {
  it("names the call a tool record guarded", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: CALL,
        systemMessage: undefined,
        outcome: { outcome: "Allowed", value: { input: undefined } },
      },
    });
    expect(toolScope(r)).toEqual(CALL);
  });

  // The split the whole rendering hangs off: a record with no call cannot
  // attach to a card, so it gets a row of its own.
  it("is null for a record with no tool call", () => {
    const r = rec({
      event: "SessionStart",
      value: {
        source: "startup",
        systemMessage: undefined,
        outcome: { outcome: "Ran", value: { additionalContext: "x" } },
      },
    });
    expect(toolScope(r)).toBeNull();
  });
});

describe("hookSummary", () => {
  it("reads a denial as an intervention", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: CALL,
        systemMessage: undefined,
        outcome: {
          outcome: "Denied",
          value: { reason: "writes are not allowed" },
        },
      },
    });
    expect(hookSummary(r)).toEqual({
      text: "writes are not allowed",
      intervened: true,
    });
  });

  // A hook that could not run denies the call, so it must read as an
  // intervention rather than as a hook that quietly passed — but distinctly
  // from a denial, because one is an outage and the other a decision.
  it("reads a failure as an intervention, distinctly from a denial", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: CALL,
        systemMessage: undefined,
        outcome: { outcome: "Failed", value: { reason: "spawn failed" } },
      },
    });
    const s = hookSummary(r);
    expect(s.intervened).toBe(true);
    expect(s.text).toContain("could not run");
  });

  it("says what a hook rewrote", () => {
    const r = rec({
      event: "PostToolUse",
      value: {
        call: CALL,
        systemMessage: undefined,
        outcome: {
          outcome: "Ran",
          value: {
            output: { before: "secret", after: "***" },
            additionalContext: undefined,
          },
        },
      },
    });
    expect(hookSummary(r).text).toContain("rewrote the output");
  });

  it("reads a no-op as allowed", () => {
    const r = rec({
      event: "PostToolUse",
      value: {
        call: CALL,
        systemMessage: undefined,
        outcome: {
          outcome: "Ran",
          value: { output: undefined, additionalContext: undefined },
        },
      },
    });
    expect(hookSummary(r)).toEqual({ text: "allowed", intervened: false });
  });

  // `Blocked` on Stop is the opposite of a refusal: the turn continues.
  it("reads a Stop block as a continuation, not a refusal", () => {
    const r = rec({
      event: "Stop",
      value: {
        systemMessage: undefined,
        outcome: {
          outcome: "Blocked",
          value: { reason: "tests still failing" },
        },
      },
    });
    const s = hookSummary(r);
    expect(s.intervened).toBe(true);
    expect(s.text).toContain("kept the turn going");
    expect(s.text).toContain("tests still failing");
  });

  it("says when the continuation cap ended the turn", () => {
    const r = rec({
      event: "Stop",
      value: {
        systemMessage: undefined,
        outcome: { outcome: "CapReached", value: { reason: "keep going" } },
      },
    });
    expect(hookSummary(r).text).toContain("continuation limit");
  });

  it("reads an injected session bootstrap as an intervention", () => {
    const r = rec({
      event: "SessionStart",
      value: {
        source: "startup",
        systemMessage: undefined,
        outcome: {
          outcome: "Ran",
          value: { additionalContext: "house rules" },
        },
      },
    });
    expect(hookSummary(r)).toEqual({
      text: "added session context",
      intervened: true,
    });
  });
});

describe("a halted hook", () => {
  // `continue: false` is a common field, so it is reported alongside whatever
  // the event itself decided rather than instead of it.
  it("reports the halt and keeps the outcome", () => {
    const r: HookRecord = {
      ...rec({
        event: "PreToolUse",
        value: {
          call: CALL,
          systemMessage: undefined,
          outcome: { outcome: "Allowed", value: { input: undefined } },
        },
      }),
      halt: { reason: "out of budget" },
    };
    expect(hookSummary(r)).toEqual({
      text: "stopped horsie — out of budget (allowed)",
      intervened: true,
    });
  });

  it("says so even with no stopReason", () => {
    const r: HookRecord = {
      ...rec({
        event: "Stop",
        value: {
          systemMessage: undefined,
          outcome: { outcome: "Ran", value: { additionalContext: undefined } },
        },
      }),
      halt: { reason: undefined },
    };
    expect(hookSummary(r).text).toContain("no reason given");
    expect(hookSummary(r).intervened).toBe(true);
  });
});

// A subagent's stop reads exactly like the session's: blocked *from stopping*.
// It used to share an arm with the objection-shaped events, so its
// `CapReached` fell through to the arm below it.
describe("SubagentStop", () => {
  it("reads a block as keeping the turn going, and a cap as the limit", () => {
    const outcome = (o: string) =>
      rec({
        event: "SubagentStop",
        value: {
          agentType: "reviewer",
          systemMessage: undefined,
          outcome: { outcome: o, value: { reason: "no tests were run" } },
        },
      } as HookRecord["action"]);
    expect(hookSummary(outcome("Blocked")).text).toContain("kept the turn going");
    expect(hookSummary(outcome("CapReached")).text).toContain(
      "continuation limit",
    );
  });
});

describe("systemMessage", () => {
  // The field that has been parsed, stored, put on the wire and read by nobody
  // since #140.
  it("is surfaced for an event that permits it", () => {
    const r = rec({
      event: "Stop",
      value: {
        systemMessage: "this repo pins node 22",
        outcome: {
          outcome: "Ran",
          value: { additionalContext: undefined },
        },
      },
    });
    expect(systemMessage(r)).toBe("this repo pins node 22");
  });

  // The side-effect-only events have no such field at all, which is a fact of
  // the type rather than a value that happens to be absent.
  it("is null for a side-effect-only event", () => {
    const r = rec({
      event: "CwdChanged",
      value: { cwd: "/work", outcome: { outcome: "Ran" } },
    });
    expect(systemMessage(r)).toBeNull();
  });
});
