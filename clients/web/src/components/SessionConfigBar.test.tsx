import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import type { AgentDocument, SessionDetail } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { settingsKey } from "../hooks/useSettings";
import { SessionConfigBar } from "./SessionConfigBar";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

/** Session facts only — the session document carries no model or agent
 * configuration, which now lives on the agent document. */
function detail(overrides: Partial<SessionDetail> = {}): SessionDetail {
  return {
    id: "s1",
    name: "Test",
    status: SessionStatusKind.Idle,
    createdAt: 0,
    annotations: [],
    vendor: "local",
    repos: [],
    plugins: [],
    usageTotal: { inputTokens: 0, outputTokens: 0 },
    agents: [],
    subSessions: [],
    ...overrides,
  };
}

/** One agent's configuration — the shape the locked row reads model, MCP,
 * memory and thinking from. */
function agent(overrides: Partial<AgentDocument> = {}): AgentDocument {
  return {
    id: "main",
    depth: 0,
    status: "idle",
    model: "sonnet",
    mcpServers: [],
    memorySpaces: [],
    usePlugins: false,
    tasks: [],
    usage: { inputTokens: 0, outputTokens: 0 },
    contextTokens: 0,
    asOfSeq: 0,
    ...overrides,
  };
}

/**
 * The locked row reads the configured models, so it needs a query client — it
 * has to know whether the selected agent's alias still exists. With no
 * settings loaded it says nothing, which is why every test below is
 * unaffected: an unknown answer must not be reported as "missing".
 */
function renderLocked(d: SessionDetail, a: AgentDocument, models?: string[]) {
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
        <SessionConfigBar mode="locked" detail={d} agent={a} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("SessionConfigBar locked mode", () => {
  // The locked row is icon-only, so every value lives in the accessible name
  // rather than in text — that is the contract a screen reader reads too.
  it("names the frozen thinking effort on its key", () => {
    const { getByTestId } = renderLocked(
      detail(),
      agent({ thinkingEffort: "high" }),
    );
    expect(getByTestId("config-thinking").getAttribute("aria-label")).toBe(
      "Thinking — high",
    );
  });

  it("omits the thinking key when no effort is set", () => {
    const { queryByTestId } = renderLocked(detail(), agent());
    expect(queryByTestId("config-thinking")).toBeNull();
  });

  it("always names the environment and the model", () => {
    const { getByTestId } = renderLocked(detail(), agent());
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
    const { getByTestId } = renderLocked(
      detail({ environment: "staging" }),
      agent(),
    );
    expect(getByTestId("config-environment").getAttribute("aria-label")).toBe(
      "Environment — staging",
    );
  });

  it("shows the locked setting name once in the model popup", () => {
    const { getByTestId, getAllByText } = renderLocked(detail(), agent());
    fireEvent.click(getByTestId("config-model"));
    expect(getAllByText("Model")).toHaveLength(1);
  });

  /**
   * A model alias can be renamed or deleted out from under a live session, and
   * the next turn then fails `no provider registered for model '…'`. The row
   * showed the dead alias exactly as it shows a live one, so the only symptom
   * was a turn that stopped working. It cannot be repaired here —
   * there is no API to repoint an existing session — but it can at least stop
   * being a surprise.
   */
  it("says so when the selected agent's model no longer exists", () => {
    const { getByTestId, getByText } = renderLocked(detail(), agent(), [
      "opus",
      "haiku",
    ]);
    expect(getByTestId("config-model").getAttribute("aria-label")).toContain(
      "missing",
    );
    fireEvent.click(getByTestId("config-model"));
    expect(getByText(/no longer configured/i)).toBeDefined();
  });

  it("stays quiet when the model is still configured", () => {
    const { getByTestId } = renderLocked(detail(), agent(), ["sonnet", "opus"]);
    expect(
      getByTestId("config-model").getAttribute("aria-label"),
    ).not.toContain("missing");
  });

  it("omits a workspace channel the session does not have", () => {
    // Skills, MCP and memory drop out when empty: five keys all reading
    // "None" is a row that says nothing.
    const { queryByTestId } = renderLocked(detail({ plugins: [] }), agent());
    expect(queryByTestId("config-skills")).toBeNull();
    const withOne = renderLocked(
      detail({ plugins: ["superpowers"] }),
      agent(),
    );
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
      agent(),
    );
    fireEvent.click(getByTestId("config-environment"));
    expect(getByText("staging")).toBeTruthy();
    expect(getByText("local")).toBeTruthy();
    // `basename` keeps the .git suffix, as it always has.
    expect(getByText("widgets.git")).toBeTruthy();
  });

  /**
   * The defect this change exists to close. A workflow run has no session
   * agent — the session document carries no model at all — so the opened
   * step's own document is the only source for the locked row. Rendering the
   * run of a `gpt-5.6-terra` plan step with the `deepseek-v4-flash` code step
   * open must read Flash, and never the start step's model.
   */
  it("shows the opened step's model, never the workflow's start step", () => {
    const run = detail({
      workflow: "two-model",
      plugins: ["superpowers"],
    });
    const code = agent({
      id: "code-step",
      title: "code",
      model: "deepseek-v4-flash",
      thinkingEffort: "high",
      mcpServers: [{ name: "mcp-code", tools: undefined }],
    });
    const { getByTestId, getByText } = renderLocked(run, code);
    const model = getByTestId("config-model");
    expect(model.getAttribute("aria-label")).toBe("Model — deepseek-v4-flash");
    expect(model.getAttribute("aria-label")).not.toContain("terra");
    // A second step-specific readout proves the row follows the opened agent's
    // configuration rather than session data.
    expect(getByTestId("config-thinking").getAttribute("aria-label")).toBe(
      "Thinking — high",
    );
    fireEvent.click(getByTestId("config-mcp"));
    expect(getByText("mcp-code")).toBeTruthy();
  });
});
