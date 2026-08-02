import { cleanup, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import type { SessionDetail } from "../api/types";
import { SessionConfigBar } from "./SessionConfigBar";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

function detail(overrides: Partial<SessionDetail> = {}): SessionDetail {
  return {
    id: "s1",
    name: "Test",
    createdAt: 0,
    pendingAsks: [],
    model: "sonnet",
    vendor: "local",
    repos: [],
    plugins: [],
    mcpServers: [],
    memorySpaces: [],
    usePlugins: false,
    inbox: [],
    ...overrides,
  };
}

function renderLocked(d: SessionDetail) {
  return render(
    <MemoryRouter>
      <SessionConfigBar mode="locked" detail={d} />
    </MemoryRouter>,
  );
}

describe("SessionConfigBar locked mode", () => {
  it("shows the frozen thinking effort as a chip", () => {
    const { getByTestId } = renderLocked(detail({ thinkingEffort: "high" }));
    const chip = getByTestId("config-thinking");
    expect(chip.textContent).toContain("high");
  });

  it("omits the thinking chip when no effort is set", () => {
    const { queryByTestId } = renderLocked(detail({ thinkingEffort: undefined }));
    expect(queryByTestId("config-thinking")).toBeNull();
  });
});
