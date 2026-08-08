import { describe, expect, it } from "vitest";
import { numberOrUndefined } from "./StepForm";

/**
 * The limits fields are the one place on the step form where "empty" and "zero"
 * are different answers: no retries is a real setting, and `Number("")` is `0` —
 * so clearing the field must not silently pin the step to zero retries.
 */
describe("numberOrUndefined", () => {
  it("treats an empty or blank field as unset", () => {
    expect(numberOrUndefined("")).toBeUndefined();
    expect(numberOrUndefined("   ")).toBeUndefined();
  });

  it("keeps zero, which is a real value for retries", () => {
    expect(numberOrUndefined("0")).toBe(0);
  });

  it("reads an ordinary number", () => {
    expect(numberOrUndefined("12")).toBe(12);
  });

  /** A number input can still hold garbage on some browsers, and a `NaN` would
   * serialize as `null` and be refused by the API. */
  it("discards anything that is not a number", () => {
    expect(numberOrUndefined("abc")).toBeUndefined();
  });
});
