import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import type { SettingsView } from "../api/types";
import { settingsKey } from "../hooks/useSettings";
import type {
  ConfigDraft,
  RuntimeChannel,
  WorkflowChannel,
} from "../hooks/useSessionDraft";
import { useConfigPickers } from "./configPickers";

// The default vendor cannot provision — the shape of a `horsie connect` setup,
// which is the case that used to leave an agent preset with no Skills or MCP
// picker at all.
const settings: SettingsView = {
  providers: [],
  models: [
    { alias: "sonnet", provider: "p", modelId: "m1" },
    { alias: "haiku", provider: "p", modelId: "m2" },
  ],
  vendors: [
    { name: "local", isDefault: true, capabilities: { supportsProvisioning: false } },
    { name: "velos", isDefault: false, capabilities: { supportsProvisioning: true } },
  ],
  defaultVendor: "local",
  info: {
    configPath: "",
    database: "",
    stateDir: "",
    dataDir: "",
    pluginsDir: "",
    version: "0",
  },
  restartRequired: false,
};

function draft(overrides: Partial<ConfigDraft> = {}): ConfigDraft {
  return {
    model: "sonnet",
    setModel: () => {},
    repos: new Map(),
    setRepos: () => {},
    skills: new Set(),
    setSkills: () => {},
    mcp: new Set(),
    setMcp: () => {},
    memorySpaces: new Set(),
    setMemorySpaces: () => {},
    thinkingEffort: "",
    setThinkingEffort: () => {},
    thinkingEfforts: [],
    modelDefaultThinkingEffort: "",
    provisions: false,
    githubConnected: false,
    ...overrides,
  };
}

function sessionDraft(overrides: Partial<ConfigDraft> = {}): ConfigDraft & RuntimeChannel {
  return { ...draft(overrides), vendor: "local", setVendor: () => {} };
}

function workflowDraft(
  workflow: string,
  overrides: Partial<ConfigDraft> = {},
): ConfigDraft & RuntimeChannel & WorkflowChannel {
  return {
    ...sessionDraft(overrides),
    workflow,
    setWorkflow: () => {},
    workflows: ["triage", "release"],
  };
}

function keys(d: ConfigDraft): string[] {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  const { result } = renderHook(() => useConfigPickers(d), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    ),
  });
  return result.current.map((p) => p.key);
}

function renderPickerBody(d: ConfigDraft, key: string) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  const { result } = renderHook(() => useConfigPickers(d), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    ),
  });
  const picker = result.current.find((p) => p.key === key);
  if (!picker) throw new Error(`missing picker ${key}`);
  return render(<MemoryRouter>{picker.body(() => {})}</MemoryRouter>);
}

describe("useConfigPickers", () => {
  // Where the work runs belongs to the invocation, not to the saved preset, so
  // an agent draft has no runtime channel and must be offered no Runtime key.
  it("offers Runtime only to a draft that has a runtime channel", () => {
    expect(keys(sessionDraft())).toContain("runtime");
    expect(keys(draft())).not.toContain("runtime");
  });

  // The regression this whole change exists for: skills and MCP are not
  // workspace channels, so a vendor that cannot provision must still offer
  // both.
  it("offers Skills and MCP on a vendor that cannot provision", () => {
    const k = keys(draft({ provisions: false }));
    expect(k).toContain("skills");
    expect(k).toContain("mcp");
  });

  // Repos is the one channel a non-provisioning vendor genuinely cannot
  // honour: there is nothing to check a repo out into.
  it("offers Repos only when the vendor provisions a workspace", () => {
    expect(keys(draft({ provisions: false }))).not.toContain("repos");
    expect(keys(draft({ provisions: true }))).toContain("repos");
  });

  it("offers Workflow only to a draft that has a workflow channel", () => {
    expect(keys(workflowDraft(""))).toContain("workflow");
    expect(keys(sessionDraft())).not.toContain("workflow");
  });

  it("puts Workflow first — it decides what the rest mean", () => {
    expect(keys(workflowDraft(""))[0]).toBe("workflow");
  });

  // A run takes its model, thinking effort, skills, MCP and memory from each
  // step's own agent preset, and the run request carries none of them. Showing
  // those controls would promise something the button cannot send.
  it("drops the agent channels once a workflow is selected", () => {
    const k = keys(workflowDraft("triage", { provisions: true }));
    expect(k).toEqual(["workflow", "runtime", "repos"]);
  });

  it("keeps every channel while no workflow is selected", () => {
    const k = keys(workflowDraft("", { provisions: true }));
    expect(k).toEqual([
      "workflow",
      "runtime",
      "repos",
      "skills",
      "mcp",
      "memory",
      "model",
    ]);
  });

  it("marks the selected runtime and model options", () => {
    const runtime = renderPickerBody(sessionDraft(), "runtime");
    expect(runtime.getByRole("button", { name: /local/ }).getAttribute("data-selected")).toBe(
      "true",
    );
    expect(runtime.getByRole("button", { name: /velos/ }).getAttribute("data-selected")).not.toBe(
      "true",
    );

    const model = renderPickerBody(sessionDraft({ model: "sonnet" }), "model");
    expect(model.getByRole("button", { name: /sonnet/ }).getAttribute("data-selected")).toBe(
      "true",
    );
    expect(model.getByRole("button", { name: /haiku/ }).getAttribute("data-selected")).not.toBe(
      "true",
    );
  });

  it("marks the selected workflow option", () => {
    const view = renderPickerBody(workflowDraft("triage"), "workflow");
    expect(view.getByRole("button", { name: /triage/ }).getAttribute("data-selected")).toBe(
      "true",
    );
    expect(view.getByRole("button", { name: /^None/ }).getAttribute("data-selected")).not.toBe(
      "true",
    );
  });

  it("highlights the selected thinking effort without replacing its radio", () => {
    const view = renderPickerBody(
      sessionDraft({ thinkingEfforts: ["low", "high"], thinkingEffort: "high" }),
      "thinking",
    );
    const high = view.getByText("high").closest("label");
    expect(high?.getAttribute("data-selected")).toBe("true");
    expect(high?.querySelector<HTMLInputElement>("input[type=radio]")?.checked).toBe(true);
  });
});
