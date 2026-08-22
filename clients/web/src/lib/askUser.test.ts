import { describe, expect, it } from "vitest";
import { composeAnswer, isAskCall, pickedChoices } from "./askUser";

describe("isAskCall", () => {
  it("recognises the session tool", () => {
    expect(isAskCall("ask_user")).toBe(true);
  });

  /** A step asks with the same tool a session does. It used to ask
   * through its finishing tool instead, so this had to sniff the payload —
   * and a step that *submitted* looked enough like a question to need telling
   * apart. Neither is true any more: the name is the whole test. */
  it("recognises a step's question by the same name", () => {
    expect(isAskCall("ask_user")).toBe(true);
  });

  it("does not treat a submitted result as a question", () => {
    expect(isAskCall("submit_result")).toBe(
      false,
    );
  });

  it("leaves every other tool alone", () => {
    expect(isAskCall("bash")).toBe(false);
    expect(isAskCall("submit_result")).toBe(false);
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
