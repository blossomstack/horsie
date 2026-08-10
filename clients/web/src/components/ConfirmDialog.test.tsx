import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { askConfirm, resetConfirm } from "../lib/confirm";
import { ConfirmDialog } from "./ConfirmDialog";

afterEach(cleanup);
afterEach(resetConfirm);

describe("ConfirmDialog", () => {
  it("renders nothing until something asks", () => {
    render(<ConfirmDialog />);
    expect(screen.queryByTestId("confirm-dialog")).toBeNull();
  });

  it("resolves true on the confirming button and false on cancel", async () => {
    render(<ConfirmDialog />);

    const accepted = askConfirm("Delete this session?");
    expect((await screen.findByTestId("confirm-dialog")).textContent).toContain(
      "Delete this session?",
    );
    fireEvent.click(screen.getByTestId("confirm-accept"));
    expect(await accepted).toBe(true);
    expect(screen.queryByTestId("confirm-dialog")).toBeNull();

    const cancelled = askConfirm("Delete this session?");
    await screen.findByTestId("confirm-dialog");
    fireEvent.click(screen.getByTestId("confirm-cancel"));
    expect(await cancelled).toBe(false);
  });

  // A confirm you cannot back out of is worse than no confirm at all.
  it("cancels on Escape and on the backdrop", async () => {
    render(<ConfirmDialog />);

    const byKey = askConfirm("Delete?");
    await screen.findByTestId("confirm-dialog");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(await byKey).toBe(false);

    const byBackdrop = askConfirm("Delete?");
    await screen.findByTestId("confirm-dialog");
    fireEvent.click(screen.getByTestId("confirm-backdrop"));
    expect(await byBackdrop).toBe(false);
  });

  // A stray second ask must not sit queued and get silently confirmed by a
  // click aimed at the first one.
  it("answers a second ask false rather than queueing it", async () => {
    render(<ConfirmDialog />);
    const first = askConfirm("First?");
    const second = askConfirm("Second?");
    expect(await second).toBe(false);
    expect((await screen.findByTestId("confirm-dialog")).textContent).toContain(
      "First?",
    );
    fireEvent.click(screen.getByTestId("confirm-accept"));
    expect(await first).toBe(true);
  });

  it("wears the caller's word on the confirming button", async () => {
    render(<ConfirmDialog />);
    void askConfirm("Remove it?", "Remove");
    expect((await screen.findByTestId("confirm-accept")).textContent).toBe(
      "Remove",
    );
  });
});
