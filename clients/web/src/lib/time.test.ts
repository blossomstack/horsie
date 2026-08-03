import { describe, expect, it } from "vitest";
import { formatDuration } from "./time";

describe("formatDuration", () => {
  it("renders sub-minute spans in seconds", () => {
    expect(formatDuration(1000)).toBe("1s");
    expect(formatDuration(45_000)).toBe("45s");
  });

  it("renders minutes with their remaining seconds", () => {
    expect(formatDuration(72_000)).toBe("1m 12s");
    expect(formatDuration(180_000)).toBe("3m 0s");
  });

  it("drops seconds past an hour", () => {
    expect(formatDuration(3_864_000)).toBe("1h 4m");
  });

  it("reports nothing for a span under a second", () => {
    // A tool that answered instantly reads better with no duration than "0s".
    expect(formatDuration(0)).toBeNull();
    expect(formatDuration(999)).toBeNull();
  });

  it("reports nothing for a nonsense span", () => {
    // Guards the subtraction at the call site: two stamps that arrive out of
    // order must not render "NaNs".
    expect(formatDuration(Number.NaN)).toBeNull();
    expect(formatDuration(-5000)).toBeNull();
  });
});
