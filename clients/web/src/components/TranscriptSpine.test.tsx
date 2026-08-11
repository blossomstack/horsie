import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RenderedCompaction } from "../hooks/useSessionStream";
import { TranscriptSpine } from "./TranscriptSpine";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// registers; without this every render piles into the same document.
afterEach(cleanup);

function boundary(seq: number): RenderedCompaction {
  return {
    seq,
    summary: "what came before",
    carriedState: "",
    covered: 12,
    tokensBefore: 100,
    tokensAfter: 10,
    manual: false,
    atMs: 0,
  };
}

describe("TranscriptSpine", () => {
  /** The control does not appear and disappear as a session's shape changes:
   * jump-to-start is useful in a long session that has never compacted. */
  it("is just the two caps with no compactions", () => {
    render(<TranscriptSpine boundaries={[]} onSeek={() => {}} />);
    expect(screen.getByTestId("spine-start")).toBeTruthy();
    expect(screen.getByTestId("spine-end")).toBeTruthy();
    expect(screen.queryAllByTestId("spine-tick")).toHaveLength(0);
  });

  it("renders one tick per boundary, carrying its seq", () => {
    render(
      <TranscriptSpine
        boundaries={[boundary(12), boundary(40)]}
        onSeek={() => {}}
      />,
    );
    const ticks = screen.getAllByTestId("spine-tick");
    expect(ticks.map((t) => t.getAttribute("data-seq"))).toEqual(["12", "40"]);
  });

  it("seeks to a boundary by seq, and to either end", () => {
    const onSeek = vi.fn();
    render(<TranscriptSpine boundaries={[boundary(12)]} onSeek={onSeek} />);
    fireEvent.click(screen.getByTestId("spine-tick"));
    expect(onSeek).toHaveBeenCalledWith(12);
    fireEvent.click(screen.getByTestId("spine-start"));
    expect(onSeek).toHaveBeenCalledWith("start");
    fireEvent.click(screen.getByTestId("spine-end"));
    expect(onSeek).toHaveBeenCalledWith("end");
  });
});
