import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CollapsibleText } from "./CollapsibleText";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** jsdom lays nothing out, so every `scrollHeight` reads 0 and no content ever
 * overflows. Overflow is staged explicitly instead. */
function stageScrollHeight(px: number) {
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(px);
}

describe("CollapsibleText", () => {
  it("renders no control when the content fits", () => {
    stageScrollHeight(100);
    render(<CollapsibleText maxHeight={320}>short</CollapsibleText>);
    expect(screen.queryByTestId("expand-text")).toBeNull();
    expect(
      screen.getByTestId("collapsible-body").style.maxHeight,
    ).toBe("");
  });

  it("ignores an overshoot too small to be worth a control", () => {
    stageScrollHeight(324);
    render(<CollapsibleText maxHeight={320}>barely over</CollapsibleText>);
    expect(screen.queryByTestId("expand-text")).toBeNull();
  });

  it("clamps and offers More when the content overflows", () => {
    stageScrollHeight(900);
    render(<CollapsibleText maxHeight={320}>long</CollapsibleText>);
    expect(screen.getByTestId("collapsible-body").style.maxHeight).toBe(
      "320px",
    );
    expect(screen.getByTestId("expand-text").textContent).toBe("More");
  });

  it("expands and collapses again", () => {
    stageScrollHeight(900);
    render(<CollapsibleText maxHeight={320}>long</CollapsibleText>);

    fireEvent.click(screen.getByTestId("expand-text"));
    expect(screen.getByTestId("collapsible-body").style.maxHeight).toBe("");
    expect(screen.getByTestId("expand-text").textContent).toBe("Less");
    expect(screen.getByTestId("expand-text").getAttribute("aria-expanded")).toBe(
      "true",
    );

    fireEvent.click(screen.getByTestId("expand-text"));
    expect(screen.getByTestId("collapsible-body").style.maxHeight).toBe(
      "320px",
    );
    expect(screen.getByTestId("expand-text").textContent).toBe("More");
  });
});
