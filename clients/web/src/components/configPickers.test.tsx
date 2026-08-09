import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, renderHook, within } from "@testing-library/react";
import type { ReactNode } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import type { EnvironmentView, SettingsView } from "../api/types";
import { environmentKeys } from "../hooks/useEnvironments";
import { settingsKey } from "../hooks/useSettings";
import type {
  ConfigDraft,
  EnvironmentChannel,
  EnvironmentDraft,
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
  defaultRuntimeVendor: "local",
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

const environments: EnvironmentView[] = [
  {
    name: "staging",
    description: "",
    vendor: "velos",
    repos: [{ url: "https://github.com/owner/api", gitRef: "dev" }],
    envVars: [],
    provision: [],
    createdAt: "1",
    updatedAt: "1",
  },
];

function draft(overrides: Partial<ConfigDraft> = {}): ConfigDraft {
  return {
    model: "sonnet",
    setModel: () => {},
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
    ...overrides,
  };
}

function sessionDraft(
  overrides: Partial<ConfigDraft & EnvironmentChannel> = {},
): ConfigDraft & EnvironmentChannel {
  const { environment, setEnvironment, provisions, githubConnected, ...rest } =
    overrides as Partial<EnvironmentChannel> & Partial<ConfigDraft>;
  return {
    ...draft(rest),
    environment:
      (environment as EnvironmentDraft | undefined) ??
      ({ kind: "runtime", vendor: "local", repos: {} } as EnvironmentDraft),
    setEnvironment: setEnvironment ?? (() => {}),
    environments,
    provisions: provisions ?? false,
    githubConnected: githubConnected ?? false,
  };
}

function workflowDraft(
  workflow: string,
  overrides: Partial<ConfigDraft & EnvironmentChannel> = {},
): ConfigDraft & EnvironmentChannel & WorkflowChannel {
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
  client.setQueryData(environmentKeys.all, environments);
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
  client.setQueryData(environmentKeys.all, environments);
  const { result } = renderHook(() => useConfigPickers(d), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    ),
  });
  const picker = result.current.find((p) => p.key === key);
  if (!picker) throw new Error(`missing picker ${key}`);
  // Scoped to this render's own container: `render` defaults its queries to
  // document.body, so two bodies rendered in one test see each other's nodes.
  const { container } = render(<MemoryRouter>{picker.body(() => {})}</MemoryRouter>);
  return within(container);
}

describe("useConfigPickers", () => {
  // Where the work runs belongs to the invocation, not to the saved preset, so
  // an agent draft has no environment channel and must be offered no key.
  it("offers Environment only to a draft that has an environment channel", () => {
    expect(keys(sessionDraft())).toContain("environment");
    expect(keys(draft())).not.toContain("environment");
  });

  // The regression this whole change exists for: skills and MCP are not
  // workspace channels, so a vendor that cannot provision must still offer
  // both.
  it("offers Skills and MCP on a vendor that cannot provision", () => {
    const k = keys(sessionDraft({ provisions: false }));
    expect(k).toContain("skills");
    expect(k).toContain("mcp");
  });

  // Repos live inside the Environment popover now — the one place the decision
  // they depend on is made — so there is no separate key at all.
  it("has no separate Repos key", () => {
    expect(keys(sessionDraft({ provisions: true }))).not.toContain("repos");
    expect(keys(sessionDraft({ provisions: true }))).not.toContain("runtime");
  });

  // The repo checklist appears only when the selection can hold repos.
  it("offers the repo checklist only on a provisioning runtime", () => {
    const off = renderPickerBody(sessionDraft({ provisions: false }), "environment");
    expect(off.queryByTestId("environment-repos")).toBeNull();
    const on = renderPickerBody(
      sessionDraft({ provisions: true, githubConnected: true }),
      "environment",
    );
    expect(on.queryByTestId("environment-repos")).not.toBeNull();
  });

  // A predefined environment's repos are part of its definition: shown, not
  // picked.
  it("shows a predefined environment's repos read-only and offers no checklist", () => {
    const view = renderPickerBody(
      sessionDraft({
        environment: { kind: "named", name: "staging" },
        provisions: true,
        githubConnected: true,
      }),
      "environment",
    );
    expect(view.queryByTestId("environment-repos")).toBeNull();
    const summary = view.getByTestId("environment-summary");
    expect(summary.textContent).toContain("api");
    expect(summary.textContent).toContain("dev");
  });

  it("lists predefined environments and connected runtimes in one control", () => {
    const view = renderPickerBody(sessionDraft(), "environment");
    const values = view
      .getAllByTestId("environment-option")
      .map((el) => `${el.getAttribute("data-kind")}:${el.getAttribute("data-value")}`);
    expect(values).toEqual(["named:staging", "runtime:local", "runtime:velos"]);
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
    expect(k).toEqual(["workflow", "environment"]);
  });

  it("keeps every channel while no workflow is selected", () => {
    const k = keys(workflowDraft("", { provisions: true }));
    expect(k).toEqual([
      "workflow",
      "environment",
      "skills",
      "mcp",
      "memory",
      "model",
    ]);
  });

  it("marks the selected environment and model options", () => {
    // By `data-value`, not by accessible name: a predefined environment's
    // label carries its vendor, so /velos/ matches two different options.
    const environment = renderPickerBody(sessionDraft(), "environment");
    const option = (value: string) =>
      environment
        .getAllByTestId("environment-option")
        .find((el) => el.getAttribute("data-value") === value);
    expect(option("local")?.getAttribute("data-selected")).toBe("true");
    expect(option("velos")?.getAttribute("data-selected")).not.toBe("true");

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
