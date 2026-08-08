import { describe, expect, it } from "vitest";
import { composeAnswer, isAskCall, pickedChoices } from "./askUser";

describe("isAskCall", () => {
  it("recognises the session tool", () => {
    expect(isAskCall("ask_user", { question: "which?" })).toBe(true);
  });

  /** A workflow step never gets `ask_user` — naming it beside `conclude` would
   * stop the loop treating `conclude` as terminal — so a step asks through
   * `conclude({kind:"ask"})`. Matching on one tool name left a parked step's
   * question rendered as a collapsed tool row with nothing to answer it with,
   * which is the whole reason a parked run could not be unblocked. */
  it("recognises a step's conclude-shaped question", () => {
    expect(isAskCall("conclude", { kind: "ask", question: "p0 or p2?" })).toBe(true);
  });

  /** The same tool submits a step's output, and that call is not a question. */
  it("is not fooled by a conclude that submits", () => {
    expect(isAskCall("conclude", { kind: "submit", output: { severity: "p0" } })).toBe(
      false,
    );
    expect(isAskCall("conclude", { severity: "p0" })).toBe(false);
    // An empty question is not a question — it would render a card with nothing
    // in it.
    expect(isAskCall("conclude", { kind: "ask", question: "" })).toBe(false);
  });

  it("leaves every other tool alone", () => {
    expect(isAskCall("bash", { command: "ls" })).toBe(false);
    expect(isAskCall("conclude", null)).toBe(false);
  });
});

describe("composeAnswer", () => {
  it("joins picked choices, then any free text", () => {
    expect(composeAnswer(["a", "b"], " why ")).toBe("a, b\n\nwhy");
    expect(composeAnswer([], "just words")).toBe("just words");
    expect(composeAnswer(["only"], "")).toBe("only");
  });
});

describe("pickedChoices", () => {
  it("recovers the selection from an answer's first block", () => {
    expect([...pickedChoices("a, b\n\nbecause", ["a", "b", "c"])]).toEqual(["a", "b"]);
  });
});
