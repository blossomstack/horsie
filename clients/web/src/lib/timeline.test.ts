import { describe, expect, it } from "vitest";
import { buildScale, GAP_PX, MAX_BAR_PX, MIN_BAR_PX } from "./timeline";

const S = (startMs: number, endMs: number) => ({ startMs, endMs });

describe("buildScale", () => {
  it("places the first span at zero", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(1000)).toBe(0);
  });

  it("keeps a short gap proportional and collapses a long one", () => {
    const kept = buildScale([S(0, 10_000), S(20_000, 30_000)]);
    const collapsed = buildScale([S(0, 10_000), S(3_610_000, 3_620_000)]);
    // A kept gap is drawn at the same scale as the spans around it: the second
    // span starts twice as far along as the first one is wide.
    expect(kept.toX(20_000)).toBeCloseTo(2 * kept.toX(10_000), 5);
    expect(collapsed.toX(3_610_000)).toBeCloseTo(collapsed.toX(10_000) + GAP_PX, 5);
  });

  it("reports what each collapsed gap swallowed", () => {
    const s = buildScale([S(0, 10_000), S(3_610_000, 3_620_000)]);
    expect(s.gaps).toHaveLength(1);
    expect(s.gaps[0].elapsedMs).toBe(3_600_000);
  });

  it("clamps a span to the minimum and maximum bar width", () => {
    const tiny = buildScale([S(0, 1)]);
    expect(tiny.toX(1) - tiny.toX(0)).toBe(MIN_BAR_PX);
    const huge = buildScale([S(0, 36_000_000)]);
    expect(huge.toX(36_000_000) - huge.toX(0)).toBe(MAX_BAR_PX);
  });

  it("clamps a moment before the start and after the end", () => {
    const s = buildScale([S(1000, 2000)]);
    expect(s.toX(0)).toBe(0);
    expect(s.toX(999_999)).toBe(s.width);
  });

  it("interpolates a moment inside a span", () => {
    const s = buildScale([S(0, 10_000)]);
    expect(s.toX(5_000)).toBeCloseTo(s.width / 2, 5);
  });

  it("keeps two zero-duration spans apart", () => {
    // Two user messages a second apart. Each still needs a clickable bar, so
    // neither may collapse onto the other.
    const s = buildScale([S(1000, 1000), S(2000, 2000)]);
    expect(s.toX(2000)).toBeGreaterThan(s.toX(1000) + MIN_BAR_PX - 1);
  });

  it("returns an empty scale for no spans", () => {
    const s = buildScale([]);
    expect(s.width).toBe(0);
    expect(s.toX(12_345)).toBe(0);
  });
});
