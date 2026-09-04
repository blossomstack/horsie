import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import type { AgentDocument, SessionDetail } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { agentKeys } from "../hooks/useAgents";
import { settingsKey } from "../hooks/useSettings";
import { workflowKeys } from "../hooks/useWorkflows";
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
    efficiency: {
            providerCalls: 0,
            toolCalls: 0,
            failedToolCalls: 0,
            toolResultBytes: 0,
            completedRuns: 0,
            abortedRuns: 0,
            compactions: 0,
          },
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
function renderLocked(
  d: SessionDetail,
  a: AgentDocument,
  models?: string[],
  server: { agents?: string[]; workflows?: string[] } = {},
) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  if (models) {
    client.setQueryData(settingsKey, {
      models: models.map((alias) => ({ alias })),
    });
  }
  if (server.agents) {
    client.setQueryData(
      agentKeys.all,
      server.agents.map((name) => ({ name })),
    );
  }
  if (server.workflows) {
    client.setQueryData(
      workflowKeys.all,
      server.workflows.map((name) => ({ name })),
    );
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

  /**
   * The row used to drop any channel that held nothing, so a session
   * deliberately narrowed to no skills looked exactly like a session nobody
   * had narrowed — and the frozen row came out a different length from the
   * draft row it is supposed to be. An empty channel is an answer; it keeps
   * its key and says "None".
   */
  it("keeps an empty workspace channel and reads None on it", () => {
    const { getByTestId } = renderLocked(detail({ plugins: [] }), agent());
    expect(getByTestId("config-skills").getAttribute("aria-label")).toBe(
      "Skills — None",
    );
  });

  it("counts a workspace channel the session does have", () => {
    const { getByTestId } = renderLocked(
      detail({ plugins: ["superpowers"] }),
      agent(),
    );
    expect(getByTestId("config-skills").getAttribute("aria-label")).toBe(
      "Skills — 1 selected",
    );
  });

  /** Every key the new-session row offers, in the same order — that is the
   * whole contract, and the one thing two separate implementations could not
   * hold on to. */
  it("shows the same channel keys a plain draft would", () => {
    const { getAllByTestId } = renderLocked(detail(), agent());
    const keys = getAllByTestId(/^config-/).map((el) =>
      el.getAttribute("data-testid"),
    );
    expect(keys).toEqual([
      "config-environment",
      "config-skills",
      "config-mcp",
      "config-memory",
      "config-tools",
      "config-model",
    ]);
  });

  /** Frozen means the controls are inert, not that they were replaced by
   * text. A checkbox you can read but not move is what says "this is the
   * same switch, already thrown". */
  it("renders its lists as controls that cannot be moved", () => {
    const { getByTestId, container } = renderLocked(
      detail({ plugins: ["superpowers"] }),
      agent(),
    );
    fireEvent.click(getByTestId("config-skills"));
    const boxes = container.querySelectorAll<HTMLInputElement>(
      'input[type="checkbox"]',
    );
    expect(boxes.length).toBeGreaterThan(0);
    for (const box of boxes) expect(box.disabled).toBe(true);
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
    // The vendor and the repo count ride beside the name, which is where the
    // draft row puts them too.
    expect(getByText(/local/)).toBeTruthy();
    // `basename` keeps the .git suffix, as it always has.
    expect(getByText("widgets.git")).toBeTruthy();
  });

  /**
   * The picker lists the live MCP catalogue, and a frozen session's selection
   * is not in it — a server can be disabled or deleted while a session that
   * uses it is still running. The row went quiet on exactly the session whose
   * MCP selection you had come to check.
   */
  it("keeps an MCP server the catalogue no longer offers", () => {
    const { getByTestId, getByText } = renderLocked(
      detail(),
      agent({ mcpServers: [{ name: "gone-from-settings" }] }),
    );
    expect(getByTestId("config-mcp").getAttribute("aria-label")).toBe(
      "MCP — 1 selected",
    );
    fireEvent.click(getByTestId("config-mcp"));
    expect(getByText("gone-from-settings")).toBeTruthy();
  });

  /**
   * Item 2, and the reason `preset` is on the wire at all.
   *
   * A session created by choosing "reviewer" was configured by one decision.
   * The row used to redraw that afterwards as six independent channels — a
   * model here, an MCP list there — reporting a configuration nobody
   * performed and losing the only fact that was actually chosen.
   */
  it("collapses a session created from a preset into its preset", () => {
    const { getByTestId, queryByTestId } = renderLocked(
      detail(),
      agent({
        preset: "reviewer",
        model: "opus",
        mcpServers: [{ name: "github" }],
        memorySpaces: ["notes"],
        thinkingEffort: "high",
      }),
      ["opus"],
      { agents: ["reviewer", "writer"] },
    );
    expect(getByTestId("config-model").getAttribute("aria-label")).toBe(
      "Model — reviewer",
    );
    // The channels the preset supplied are not separate decisions, so they
    // are not separate keys.
    for (const key of ["config-mcp", "config-memory", "config-tools", "config-thinking"]) {
      expect(queryByTestId(key)).toBeNull();
    }
  });

  /** Collapsing must not hide. Everything the exploded row used to show is
   * inside the one key, under the name that was actually chosen. */
  it("reads out what the preset resolved to", () => {
    const { getByTestId } = renderLocked(
      detail({ plugins: ["superpowers"] }),
      agent({
        preset: "reviewer",
        model: "opus",
        mcpServers: [{ name: "github" }],
        memorySpaces: ["notes"],
        thinkingEffort: "high",
      }),
      ["opus"],
      { agents: ["reviewer"] },
    );
    fireEvent.click(getByTestId("config-model"));
    expect(getByTestId("resolved-model").textContent).toBe("opus");
    expect(getByTestId("resolved-mcp").textContent).toBe("github");
    expect(getByTestId("resolved-memory").textContent).toBe("notes");
    expect(getByTestId("resolved-thinking").textContent).toBe("high");
    expect(getByTestId("resolved-tools").textContent).toBe("Default");
  });

  /** A preset can be deleted while a session created from it is still
   * running. Its settings were flattened at creation and still apply, so the
   * session is fine — but a name that no longer resolves should say so. */
  it("says so when the preset has been deleted", () => {
    const { getByTestId, getByText } = renderLocked(
      detail(),
      agent({ preset: "reviewer", model: "opus" }),
      ["opus"],
      { agents: ["writer"] },
    );
    expect(getByTestId("config-model").getAttribute("aria-label")).toContain(
      "deleted",
    );
    fireEvent.click(getByTestId("config-model"));
    expect(getByText(/no longer exists/i)).toBeDefined();
  });

  /** A workflow step is two facts at once: a step of a run, and an instance
   * of its own preset. Choosing between them is what the draft row does,
   * because there they are alternatives; frozen, both are true. */
  it("names both the run's workflow and the step's own preset", () => {
    const { getByTestId } = renderLocked(
      detail({ workflow: "release" }),
      agent({ id: "code-step", title: "code", preset: "coder", model: "opus" }),
      ["opus"],
      { agents: ["coder"], workflows: ["release"] },
    );
    expect(getByTestId("config-workflow").getAttribute("aria-label")).toBe(
      "Workflow — release",
    );
    expect(getByTestId("config-model").getAttribute("aria-label")).toBe(
      "Model — coder",
    );
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
