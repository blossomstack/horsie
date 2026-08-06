import { describe, expect, it } from "vitest";
import { liveTurnIndex, type TurnGroup } from "./Transcript";

const user = (id: string, pending?: "queued" | "optimistic"): TurnGroup => ({
  kind: "user",
  msg: {
    id,
    role: "User",
    text: "hi",
    thinking: [],
    toolCalls: [],
    subagentResults: [],
    queued: pending === "queued",
    optimistic: pending === "optimistic",
  },
});

const assistant = (id: string): TurnGroup => ({
  kind: "assistant",
  id,
  msgs: [
    {
      id,
      role: "Assistant",
      text: "",
      thinking: [],
      toolCalls: [],
      subagentResults: [],
    },
  ],
});

describe("liveTurnIndex", () => {
  it("is the assistant turn when it is the last one", () => {
    expect(liveTurnIndex([user("u1"), assistant("a1")])).toBe(1);
  });

  // The bug this exists for: a message sent while the agent is working is
  // appended after its turn, so the last turn is a user bubble. Deciding the
  // live turn by position handed the live tail to that bubble and left the
  // running work group rendering its past-tense summary — "Ran 2 tools" over a
  // tool that had five seconds left.
  it("stays on the running assistant turn when a queued message follows it", () => {
    expect(
      liveTurnIndex([user("u1"), assistant("a1"), user("u2", "queued")]),
    ).toBe(1);
  });

  // The other half of the same rule, and the one that keeps a *new* turn's
  // output from being drawn above the message that asked for it. A message the
  // server has consumed is part of the conversation; whatever the agent says
  // next belongs after it, not folded into the turn before it.
  it("is null when a consumed message follows the last assistant turn", () => {
    expect(liveTurnIndex([user("u1"), assistant("a1"), user("u2")])).toBeNull();
  });

  it("is the most recent assistant turn, not an earlier one", () => {
    expect(
      liveTurnIndex([user("u1"), assistant("a1"), user("u2"), assistant("a2")]),
    ).toBe(3);
  });

  it("is null before the agent has produced anything", () => {
    expect(liveTurnIndex([user("u1")])).toBeNull();
  });
});
