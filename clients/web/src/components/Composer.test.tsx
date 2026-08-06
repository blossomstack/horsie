import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionStatusKind, type CatalogEntryView } from "../api/types";
import { Composer } from "./Composer";
import { filterEntries, invocationPrefix } from "./EntryMenu";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

const entries: CatalogEntryView[] = [
  {
    kind: "command",
    name: "commit",
    description: "Create a git commit",
    argumentHint: "<msg>",
  },
  { kind: "command", name: "review", description: "Review a pull request" },
  { kind: "agent", name: "reviewer", description: "Reviews a diff" },
];

function composer(onSend = vi.fn()) {
  render(
    <Composer
      status={SessionStatusKind.Idle}
      busy={false}
      entries={entries}
      onSend={onSend}
      onStop={vi.fn()}
    />,
  );
  return {
    input: screen.getByTestId("composer-input"),
    onSend,
  };
}

describe("invocationPrefix", () => {
  it("matches a leading sigil and only while the name is being typed", () => {
    expect(invocationPrefix("/")).toEqual({ sigil: "/", query: "" });
    expect(invocationPrefix("/rev")).toEqual({ sigil: "/", query: "rev" });
    expect(invocationPrefix("@rev")).toEqual({ sigil: "@", query: "rev" });
    // Arguments have started: the user has moved on.
    expect(invocationPrefix("/review src")).toBeNull();
    // Not leading — the same rule the server's parser uses.
    expect(invocationPrefix("see /review")).toBeNull();
    expect(invocationPrefix("mail a@b.com")).toBeNull();
    expect(invocationPrefix("hello")).toBeNull();
  });
});

describe("filterEntries", () => {
  it("splits by sigil so `@` is not a second `/`", () => {
    expect(filterEntries(entries, "@", "").map((e) => e.name)).toEqual([
      "reviewer",
    ]);
    expect(filterEntries(entries, "/", "").map((e) => e.name)).toEqual([
      "commit",
      "review",
    ]);
  });

  it("matches on name or description", () => {
    expect(filterEntries(entries, "/", "rev").map((e) => e.name)).toEqual([
      "review",
    ]);
    expect(filterEntries(entries, "/", "git").map((e) => e.name)).toEqual([
      "commit",
    ]);
  });
});

describe("Composer typeahead", () => {
  it("opens on a leading slash and not mid-message", () => {
    const { input } = composer();
    fireEvent.change(input, { target: { value: "look at /etc" } });
    expect(screen.queryByTestId("entry-menu")).toBeNull();

    fireEvent.change(input, { target: { value: "/" } });
    expect(screen.getByTestId("entry-menu")).toBeTruthy();
    expect(screen.getByText("/commit")).toBeTruthy();
    // An agent is not reachable from `/`.
    expect(screen.queryByText("@reviewer")).toBeNull();
  });

  /// Enter with the menu open must pick, not send: sending `/rev` because the
  /// menu was up is the mistake the key ordering exists to prevent.
  it("Enter picks while the menu is open and sends once it is closed", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/rev" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onSend).not.toHaveBeenCalled();
    expect((input as HTMLTextAreaElement).value).toBe("/review ");
    expect(screen.queryByTestId("entry-menu")).toBeNull();

    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/review");
  });

  it("moves the selection with the arrow keys", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Tab" });
    expect((input as HTMLTextAreaElement).value).toBe("/review ");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("dismisses with Escape without sending", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/rev" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByTestId("entry-menu")).toBeNull();
    expect(onSend).not.toHaveBeenCalled();
  });

  it("stays out of the way when nothing matches", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/nosuch" } });
    expect(screen.queryByTestId("entry-menu")).toBeNull();
    // And Enter sends it verbatim — an unknown name is not an error.
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/nosuch");
  });
});
