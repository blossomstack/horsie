import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { PluginView } from "../../../api/types";
import { BundleRow } from "./BundleRow";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

vi.mock("../../../hooks/usePlugins", () => ({
  useSetPluginDefault: () => ({ mutate: vi.fn(), isPending: false }),
  useUpdatePlugin: () => ({ mutate: vi.fn(), isPending: false }),
  useRemovePlugin: () => ({ mutate: vi.fn(), isPending: false }),
}));

function bundle(over: Partial<PluginView> = {}): PluginView {
  return {
    name: "feature-dev",
    description: "d",
    version: "1.0.0",
    kind: {
      kind: "Claude",
      value: {
        url: "https://example.com/x.git",
        gitRef: undefined,
        subpath: undefined,
        marketplace: undefined,
        marketplaceEntry: undefined,
      },
    },
    catalog: [],
    hasHooks: false,
    enabledDefault: false,
    artifactSize: 1,
    ...over,
  };
}

describe("BundleRow", () => {
  it("counts each kind and omits the ones with nothing", () => {
    render(
      <BundleRow
        bundle={bundle({
          catalog: [
            { kind: "command", name: "commit", description: "c" },
            { kind: "command", name: "review", description: "r" },
            { kind: "skill", name: "tdd", description: "t" },
          ],
        })}
      />,
    );
    // No agents, so no "0 agents".
    expect(screen.getByText("2 commands · 1 skill")).toBeTruthy();
  });

  it("expands to the exact strings a user types", () => {
    render(
      <BundleRow
        bundle={bundle({
          catalog: [
            {
              kind: "command",
              name: "commit",
              description: "Create a git commit",
              argumentHint: "<msg>",
            },
            { kind: "agent", name: "reviewer", description: "Reviews a diff" },
          ],
        })}
      />,
    );
    // Collapsed until asked.
    expect(screen.queryByText("/commit")).toBeNull();

    fireEvent.click(screen.getByRole("button", { expanded: false }));

    expect(screen.getByText("/commit")).toBeTruthy();
    expect(screen.getByText("<msg>")).toBeTruthy();
    expect(screen.getByText("Create a git commit")).toBeTruthy();
    // An agent is reached with `@`, and showing `/reviewer` would be a lie.
    expect(screen.getByText("@reviewer")).toBeTruthy();
  });

  it("cannot be expanded when the bundle offers nothing", () => {
    render(<BundleRow bundle={bundle({ catalog: [], hasHooks: true })} />);
    const toggle = screen.getByRole("button", { expanded: false });
    expect((toggle as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("nothing horsie runs")).toBeTruthy();
  });

  /// Empty is where every authored plugin starts, and where it sits while it
  /// is being filled in — so it reads as a beginning, not as a fault.
  it("says an empty authored bundle has no skills yet", () => {
    render(
      <BundleRow
        bundle={bundle({
          catalog: [],
          kind: { kind: "Authored", value: { generation: 1 } },
        })}
      />,
    );
    expect(screen.getByText("no skills written yet")).toBeTruthy();
  });

  /// An authored bundle has no upstream, and its library row is a projection
  /// of the skills it renders from — so the server refuses both to update it
  /// and to uninstall it. A button that can only ever produce an error is
  /// worse than no button.
  it("offers Update and Delete for a clone but not for an authored bundle", () => {
    const { unmount } = render(<BundleRow bundle={bundle()} />);
    expect(screen.queryByText("Update")).not.toBeNull();
    expect(screen.queryByLabelText("Delete bundle")).not.toBeNull();
    unmount();

    render(
      <BundleRow
        bundle={bundle({
          kind: { kind: "Authored", value: { generation: 3 } },
        })}
      />,
    );
    expect(screen.queryByText("Update")).toBeNull();
    expect(screen.queryByLabelText("Delete bundle")).toBeNull();
  });
});
