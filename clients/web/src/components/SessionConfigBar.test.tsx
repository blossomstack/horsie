import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import type { SessionDetail } from "../api/types";
import { settingsKey } from "../hooks/useSettings";
import { SessionConfigBar } from "./SessionConfigBar";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

function detail(overrides: Partial<SessionDetail> = {}): SessionDetail {
  return {
    id: "s1",
    name: "Test",
    createdAt: 0,
    annotations: [],
    model: "sonnet",
    vendor: "local",
    repos: [],
    plugins: [],
    mcpServers: [],
    memorySpaces: [],
    usePlugins: false,
    usageTotal: { inputTokens: 0, outputTokens: 0 },
    agents: [],
    ...overrides,
  };
}

/**
 * The locked row reads the configured models, so it needs a query client — it
 * has to know whether the session's alias still exists. With no settings
 * loaded it says nothing, which is why every test below is unaffected: an
 * unknown answer must not be reported as "missing".
 */
function renderLocked(d: SessionDetail, models?: string[]) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  if (models) {
    client.setQueryData(settingsKey, {
      models: models.map((alias) => ({ alias })),
    });
  }
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <SessionConfigBar mode="locked" detail={d} />
      </MemoryRouter>
    </QueryClientProvider>,
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

  it("always names the environment and the model", () => {
    const { getByTestId } = renderLocked(detail());
    // An ad-hoc environment has no name of its own, so the key reads the
    // vendor it resolved to.
    expect(getByTestId("config-environment").getAttribute("aria-label")).toBe(
      "Environment — local",
    );
    expect(getByTestId("config-model").getAttribute("aria-label")).toBe(
      "Model — sonnet",
    );
  });

  it("leads with the predefined environment when the session named one", () => {
    const { getByTestId } = renderLocked(detail({ environment: "staging" }));
    expect(getByTestId("config-environment").getAttribute("aria-label")).toBe(
      "Environment — staging",
    );
  });

  it("shows the locked setting name once in the model popup", () => {
    const { getByTestId, getAllByText } = renderLocked(detail());
    fireEvent.click(getByTestId("config-model"));
    expect(getAllByText("Model")).toHaveLength(1);
  });

  /**
   * A model alias can be renamed or deleted out from under a live session, and
   * the next turn then fails `no provider registered for model '…'`. The row
   * showed the dead alias exactly as it shows a live one, so the only symptom
   * was a session that had quietly stopped working.
   */
  it("says so when the session's model no longer exists", () => {
    const { getByTestId, getByText } = renderLocked(detail(), ["opus", "haiku"]);
    expect(getByTestId("config-model").getAttribute("aria-label")).toContain(
      "missing",
    );
    fireEvent.click(getByTestId("config-model"));
    expect(getByText(/no longer configured/i)).toBeDefined();
  });

  it("stays quiet when the model is still configured", () => {
    const { getByTestId } = renderLocked(detail(), ["sonnet", "opus"]);
    expect(
      getByTestId("config-model").getAttribute("aria-label"),
    ).not.toContain("missing");
  });

  it("omits a workspace channel the session does not have", () => {
    // Skills, MCP and memory drop out when empty: five keys all reading
    // "None" is a row that says nothing.
    const { queryByTestId } = renderLocked(detail({ plugins: [] }));
    expect(queryByTestId("config-skills")).toBeNull();
    const withOne = renderLocked(detail({ plugins: ["superpowers"] }));
    expect(withOne.queryByTestId("config-skills")).not.toBeNull();
  });

  // Repos are no longer a key of their own — they are what the environment
  // resolved to, so they read inside it, exactly as they are picked.
  it("reads the environment's vendor and repos inside its own key", () => {
    const { getByTestId, getByText } = renderLocked(
      detail({
        environment: "staging",
        repos: ["https://github.com/acme/widgets.git"],
      }),
    );
    fireEvent.click(getByTestId("config-environment"));
    expect(getByText("staging")).toBeTruthy();
    expect(getByText("local")).toBeTruthy();
    // `basename` keeps the .git suffix, as it always has.
    expect(getByText("widgets.git")).toBeTruthy();
  });
});
