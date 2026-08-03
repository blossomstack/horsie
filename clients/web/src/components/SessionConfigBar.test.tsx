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
    usageTotal: { inputTokens: 0, outputTokens: 0 },
    agents: [],
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
  // The locked row is icon-only, so every value lives in the accessible name
  // rather than in text — that is the contract a screen reader reads too.
  it("names the frozen thinking effort on its key", () => {
    const { getByTestId } = renderLocked(detail({ thinkingEffort: "high" }));
    expect(getByTestId("config-thinking").getAttribute("aria-label")).toBe(
      "Thinking — high",
    );
  });

  it("omits the thinking key when no effort is set", () => {
    const { queryByTestId } = renderLocked(detail({ thinkingEffort: undefined }));
    expect(queryByTestId("config-thinking")).toBeNull();
  });

  it("always names the runtime and the model", () => {
    const { getByTestId } = renderLocked(detail());
    expect(getByTestId("config-runtime").getAttribute("aria-label")).toBe(
      "Runtime — local",
    );
    expect(getByTestId("config-model").getAttribute("aria-label")).toBe(
      "Model — sonnet",
    );
  });

  it("omits a workspace channel the session does not have", () => {
    // A non-provisioning vendor has no repos, and five keys all reading
    // "None" is a row that says nothing.
    const { queryByTestId } = renderLocked(detail({ repos: [] }));
    expect(queryByTestId("config-repos")).toBeNull();
  });

  it("shows a workspace channel the session does have", () => {
    const { getByTestId } = renderLocked(
      detail({ repos: ["https://github.com/acme/widgets.git"] }),
    );
    expect(getByTestId("config-repos").getAttribute("aria-label")).toBe(
      // `basename` keeps the .git suffix, as it always has.
      "Repos — widgets.git",
    );
  });
});
