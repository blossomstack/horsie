import { afterEach, describe, expect, it, vi } from "vitest";
import { relativeTime } from "./format";

const NOW = Date.UTC(2026, 7, 9, 12, 0, 0);

function at(now: number) {
  vi.useFakeTimers();
  vi.setSystemTime(now);
}

afterEach(() => {
  vi.useRealTimers();
});

describe("relativeTime", () => {
  it("reads a past timestamp as elapsed", () => {
    at(NOW);
    expect(relativeTime(NOW - 3 * 60_000)).toBe("3m ago");
    expect(relativeTime(NOW - 2 * 3_600_000)).toBe("2h ago");
    expect(relativeTime(NOW - 3 * 86_400_000)).toBe("3d ago");
    expect(relativeTime(NOW - 5_000)).toBe("just now");
  });

  // The routine "next run" bug: a future timestamp gave a negative diff that
  // fell into the "< 45s" branch, so every armed routine read "next just now".
  it("reads a future timestamp as remaining, not as 'just now'", () => {
    at(NOW);
    expect(relativeTime(NOW + 3 * 60_000)).toBe("in 3m");
    expect(relativeTime(NOW + 2 * 3_600_000)).toBe("in 2h");
    expect(relativeTime(NOW + 3 * 86_400_000)).toBe("in 3d");
    expect(relativeTime(NOW + 5_000)).toBe("in a moment");
  });

  it("falls back to a date once either direction is a week out", () => {
    at(NOW);
    expect(relativeTime(NOW - 30 * 86_400_000)).not.toMatch(/ago/);
    expect(relativeTime(NOW + 30 * 86_400_000)).not.toMatch(/^in /);
  });
});
