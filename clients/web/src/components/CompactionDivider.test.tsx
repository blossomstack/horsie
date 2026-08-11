import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { RenderedCompaction } from "../hooks/useSessionStream";
import { CompactionDivider } from "./CompactionDivider";

afterEach(cleanup);

const value: RenderedCompaction = {
  seq: 12,
  summary: "they migrated the journal",
  carriedState: "Tasks (0/1 done):\n[ ] 1. finish the migration",
  covered: 41,
  tokensBefore: 120_000,
  tokensAfter: 12_000,
  manual: false,
  atMs: 0,
};

describe("CompactionDivider", () => {
  /** The summary is long by construction; what matters at a glance is that a
   * boundary is here and what it bought. */
  it("is collapsed until asked", () => {
    render(<CompactionDivider value={value} />);
    expect(
      screen.getByTestId("compaction-divider").getAttribute("data-seq"),
    ).toBe("12");
    expect(screen.queryByTestId("compaction-detail")).toBeNull();
    fireEvent.click(screen.getByTestId("compaction-toggle"));
    expect(screen.getByTestId("compaction-detail")).toBeTruthy();
  });

  /** The two halves differ in kind, not just content: one is the model's prose
   * and may be wrong, the other is exact and never went near the summariser. */
  it("shows the summary and the carried state apart", () => {
    render(<CompactionDivider value={value} />);
    fireEvent.click(screen.getByTestId("compaction-toggle"));
    const detail = screen.getByTestId("compaction-detail");
    expect(detail.textContent).toContain("they migrated the journal");
    expect(detail.textContent).toContain("finish the migration");
  });

  it("says when a compaction was asked for rather than automatic", () => {
    render(<CompactionDivider value={{ ...value, manual: true }} />);
    expect(screen.getByTestId("compaction-toggle").textContent).toContain(
      "by hand",
    );
  });
});
