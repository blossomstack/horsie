import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AuthoredPluginView } from "../../../api/types";
import { AuthoredSection } from "./AuthoredSection";

afterEach(cleanup);

const revisions = {
  current: [
    { revision: 2, description: "second", body: "b", files: [], deleted: false, createdAt: "2" },
    { revision: 1, description: "first", body: "a", files: [], deleted: false, createdAt: "1" },
  ],
};
const restore = vi.fn();

vi.mock("../../../hooks/useAuthored", () => ({
  useCreateAuthoredPlugin: () => ({ mutate: vi.fn(), isPending: false, isError: false }),
  useRemoveAuthoredPlugin: () => ({ mutate: vi.fn(), isPending: false }),
  useRemoveSkill: () => ({ mutate: vi.fn(), isPending: false }),
  useRestoreSkill: () => ({ mutate: restore, isPending: false }),
  useSkillRevisions: () => ({
    data: revisions.current,
    isLoading: false,
    isError: false,
  }),
}));

function plugin(over: Partial<AuthoredPluginView> = {}): AuthoredPluginView {
  return {
    name: "field-notes",
    description: "what I worked out",
    generation: 3,
    skills: [
      {
        plugin: "field-notes",
        name: "rolling-back",
        description: "How to roll back",
        revision: 2,
        updatedAt: "1",
      },
    ],
    ...over,
  };
}

describe("AuthoredSection", () => {
  it("shows the generation and the skills behind a disclosure", () => {
    render(<AuthoredSection plugins={[plugin()]} />);
    expect(screen.getByText("field-notes")).toBeDefined();
    expect(screen.getByText("gen 3")).toBeDefined();
    // Collapsed: the skill is not on screen until its plugin is opened.
    expect(screen.queryByTestId("authored-skill")).toBeNull();

    fireEvent.click(screen.getByText("1 skill"));
    expect(screen.getByText("rolling-back")).toBeDefined();
    expect(screen.getByText("How to roll back")).toBeDefined();
  });

  /// The history is the reason the rows are append-only, so it has to be
  /// reachable — and the current revision must not offer a restore that would
  /// cost a generation and change nothing.
  it("offers a restore for older revisions only", () => {
    render(<AuthoredSection plugins={[plugin()]} />);
    fireEvent.click(screen.getByText("1 skill"));
    fireEvent.click(screen.getByLabelText("History of rolling-back"));

    const buttons = screen.getAllByText("restore");
    expect(buttons.length).toBe(1);
    fireEvent.click(buttons[0]!);
    expect(restore).toHaveBeenCalledWith({
      plugin: "field-notes",
      skill: "rolling-back",
      revision: 1,
    });
  });

  it("says so when nothing has been authored", () => {
    render(<AuthoredSection plugins={[]} />);
    expect(screen.getByText(/Nothing authored yet/)).toBeDefined();
  });
});
