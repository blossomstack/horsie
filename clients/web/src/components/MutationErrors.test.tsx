import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  pushMutationError,
  resetMutationErrors,
} from "../api/mutationErrors";
import { MutationErrors } from "./MutationErrors";

afterEach(cleanup);
beforeEach(() => resetMutationErrors());

describe("MutationErrors", () => {
  it("renders nothing until a write fails", () => {
    render(<MutationErrors />);
    expect(screen.queryByTestId("mutation-errors")).toBeNull();
  });

  it("shows the server's own message, which is the whole point", () => {
    render(<MutationErrors />);
    // `409 agent_in_use` names the routine that is blocking the delete, and
    // this text vanishing is exactly what was observed.
    act(() => pushMutationError("routine 'nightly' uses this agent"));
    expect(
      screen.getByText("routine 'nightly' uses this agent"),
    ).toBeDefined();
    // Never colour alone.
    expect(screen.getByText("Failed")).toBeDefined();
  });

  it("dismisses one notice without touching the others", () => {
    render(<MutationErrors />);
    act(() => pushMutationError("first"));
    act(() => pushMutationError("second"));
    expect(screen.getAllByTestId("mutation-error")).toHaveLength(2);

    fireEvent.click(screen.getAllByTestId("mutation-error-dismiss")[0]);
    expect(screen.getAllByTestId("mutation-error")).toHaveLength(1);
    expect(screen.getByText("second")).toBeDefined();
  });

  it("does not stack a retry that keeps failing the same way", () => {
    render(<MutationErrors />);
    act(() => pushMutationError("same"));
    act(() => pushMutationError("same"));
    act(() => pushMutationError("same"));
    expect(screen.getAllByTestId("mutation-error")).toHaveLength(1);
  });

  it("keeps only the most recent few, so a burst is not a wall", () => {
    render(<MutationErrors />);
    act(() => {
      for (const m of ["a", "b", "c", "d", "e"]) pushMutationError(m);
    });
    const shown = screen.getAllByTestId("mutation-error");
    expect(shown).toHaveLength(3);
    expect(screen.queryByText("a")).toBeNull();
    expect(screen.getByText("e")).toBeDefined();
  });
});
