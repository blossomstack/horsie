import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, renderHook, within } from "@testing-library/react";
import type { ReactNode } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import { ApiRequestError } from "../api/client";
import type { EnvironmentView, SettingsView, ToolCatalog } from "../api/types";
import { ToolAccess } from "../api/types";
import { environmentKeys } from "../hooks/useEnvironments";
import { memorySpacesKey } from "../hooks/useMemory";
import { pluginsKey } from "../hooks/usePlugins";
import { settingsKey } from "../hooks/useSettings";
import { toolsKey } from "../hooks/useTools";
import type {
  AgentChannel,
  ConfigDraft,
  EnvironmentChannel,
  EnvironmentDraft,
  WorkflowChannel,
} from "../hooks/useSessionDraft";
import { useConfigPickers, type PickerSpec } from "./configPickers";

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
    tools: null,
    setTools: () => {},
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

function agentDraft(agent: string): ConfigDraft & EnvironmentChannel & AgentChannel & WorkflowChannel {
  return {
    ...workflowDraft(""),
    agent,
    setAgent: () => {},
    agents: ["reviewer"],
  };
}

// Two groups, one of which is out of the default set — the shape that makes
// "Default" and "All" different answers, which is the whole point of the
// tri-state.
const toolCatalog: ToolCatalog = {
  groups: [
    {
      key: "runtime",
      label: "Files & shell",
      description: "Read and change files.",
      tools: [
        { name: "bash", description: "Run a command.", access: ToolAccess.Write, inDefaultSet: true },
        { name: "read_file", description: "Read a file.", access: ToolAccess.Read, inDefaultSet: true },
      ],
    },
    {
      key: "control",
      label: "horsie",
      description: "Manage this server.",
      tools: [
        {
          name: "horsie_agents",
          description: "Manage agents.",
          access: ToolAccess.Write,
          inDefaultSet: false,
        },
      ],
    },
  ],
};

function keys(d: ConfigDraft): string[] {
  return pickers(d, seededClient()).map((p) => p.key);
}

function seededClient(): QueryClient {
  const client = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        staleTime: Infinity,
        // An observer mounting on an errored query refetches by default, which
        // would drop a `failRead` fixture back to "loading" before the first
        // assertion.
        refetchOnMount: false,
        retryOnMount: false,
      },
    },
  });
  client.setQueryData(settingsKey, settings);
  client.setQueryData(environmentKeys.all, environments);
  client.setQueryData(toolsKey, toolCatalog);
  return client;
}

/** Leave a query in the state a failed read leaves behind. */
function failRead(client: QueryClient, key: readonly unknown[]): QueryClient {
  client
    .getQueryCache()
    .build(client, { queryKey: key })
    .setState({
      status: "error",
      fetchStatus: "idle",
      error: new ApiRequestError(
        0,
        "network",
        "Could not reach the horsie server. Is `horsie serve` running?",
      ),
    });
  return client;
}

function pickers(d: ConfigDraft, client: QueryClient): PickerSpec[] {
  const { result } = renderHook(() => useConfigPickers(d), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    ),
  });
  return result.current;
}

function renderPickerBody(
  d: ConfigDraft,
  key: string,
  client: QueryClient = seededClient(),
) {
  const picker = pickers(d, client).find((p) => p.key === key);
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

  it("hides workflow and direct agent channels for a selected agent", () => {
    expect(keys(agentDraft("reviewer"))).toEqual(["environment", "model"]);
  });

  it("lists agents separately from models in the model menu", () => {
    const view = renderPickerBody(agentDraft(""), "model");
    expect(view.getByText("Models")).toBeTruthy();
    expect(view.getByText("Agents")).toBeTruthy();
    expect(view.getByTestId("agent-option").getAttribute("data-value")).toBe("reviewer");
  });

  it("keeps every channel while no workflow is selected", () => {
    const k = keys(workflowDraft("", { provisions: true }));
    expect(k).toEqual([
      "workflow",
      "environment",
      "skills",
      "mcp",
      "memory",
      "tools",
      "model",
    ]);
  });

  // The tri-state the whole channel turns on. "Default" is not "all ticked":
  // it defers to the server, which is what keeps a preset following a later
  // horsie's idea of sensible — and what stops an unset field granting the
  // control plane.
  it("shows the default set ticked while nothing has been chosen", () => {
    const spec = pickers(draft(), seededClient()).find((p) => p.key === "tools");
    expect(spec?.label).toBe("Default");
    expect(spec?.marked).toBe(false);

    const view = renderPickerBody(draft(), "tools");
    const option = (name: string) =>
      view.getAllByTestId("tool-option").find((el) => el.getAttribute("data-value") === name);
    expect(option("bash")?.getAttribute("data-selected")).toBe("true");
    expect(option("horsie_agents")?.getAttribute("data-selected")).toBe("false");
  });

  it("labels an explicit selection by size, and marks the channel", () => {
    const spec = pickers(draft({ tools: new Set(["bash"]) }), seededClient()).find(
      (p) => p.key === "tools",
    );
    expect(spec?.label).toBe("1 selected");
    expect(spec?.marked).toBe(true);
  });

  // The two answers that are easy to collapse into one, and must not be.
  it("tells an empty selection apart from an unmade one", () => {
    const none = pickers(draft({ tools: new Set() }), seededClient()).find(
      (p) => p.key === "tools",
    );
    expect(none?.label).toBe("None");
    expect(none?.marked).toBe(true);
  });

  it("groups tools and badges each one read or write", () => {
    const view = renderPickerBody(draft(), "tools");
    expect(view.getByTestId("tool-group-runtime")).toBeTruthy();
    expect(view.getByTestId("tool-group-control")).toBeTruthy();
    const badges = view.getAllByTestId("tool-access").map((el) => el.getAttribute("data-access"));
    expect(badges).toEqual(["write", "read", "write"]);
  });

  it("offers quick selections, including a read-only one", () => {
    const chosen: (Set<string> | null)[] = [];
    const d = draft({ setTools: (t) => chosen.push(t) });
    const view = renderPickerBody(d, "tools");

    view.getByTestId("tool-quick-read").click();
    expect(chosen.at(-1)).toEqual(new Set(["read_file"]));

    view.getByTestId("tool-quick-all").click();
    expect(chosen.at(-1)).toEqual(new Set(["bash", "read_file", "horsie_agents"]));

    view.getByTestId("tool-quick-none").click();
    expect(chosen.at(-1)).toEqual(new Set());

    // Back to deferring, which no set of ticks can express.
    view.getByTestId("tool-quick-default").click();
    expect(chosen.at(-1)).toBeNull();
  });

  it("ticks and unticks a whole group at once", () => {
    const chosen: (Set<string> | null)[] = [];
    const d = draft({ tools: new Set(), setTools: (t) => chosen.push(t) });
    const view = renderPickerBody(d, "tools");
    view.getByTestId("tool-group-all-runtime").click();
    expect(chosen.at(-1)).toEqual(new Set(["bash", "read_file"]));
  });

  it("says so when the catalogue cannot be read", () => {
    const view = renderPickerBody(
      draft(),
      "tools",
      failRead(seededClient(), toolsKey),
    );
    expect(view.getByTestId("tools-read-error")).toBeTruthy();
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

  // A dead `/api/config` is not an account with nothing configured. The whole
  // point of these three is the distinction: same empty list, different cause,
  // and only one of them is something the reader can act on by adding a model.
  it("says the model list failed to load rather than that there are none", () => {
    const failed = renderPickerBody(
      sessionDraft(),
      "model",
      failRead(seededClient(), settingsKey),
    );
    expect(failed.getByTestId("model-read-error").textContent).toContain(
      "Couldn’t load models",
    );
    expect(failed.queryByText(/No models configured/)).toBeNull();

    const empty = renderPickerBody(sessionDraft(), "model", (() => {
      const c = seededClient();
      c.setQueryData(settingsKey, { ...settings, models: [] });
      return c;
    })());
    expect(empty.queryByTestId("model-read-error")).toBeNull();
    expect(empty.queryByText(/No models configured/)).not.toBeNull();
  });

  it("says the runtime list failed to load rather than that none is connected", () => {
    const failed = renderPickerBody(
      sessionDraft(),
      "environment",
      failRead(seededClient(), settingsKey),
    );
    expect(failed.getByTestId("environment-read-error").textContent).toContain(
      "Couldn’t load runtimes",
    );
    expect(failed.queryByText(/No runtime is connected/)).toBeNull();

    const empty = renderPickerBody(sessionDraft(), "environment", (() => {
      const c = seededClient();
      c.setQueryData(settingsKey, { ...settings, vendors: [] });
      return c;
    })());
    expect(empty.queryByTestId("environment-read-error")).toBeNull();
    expect(empty.queryByText(/No runtime is connected/)).not.toBeNull();
  });

  it("says a failed toolbox read failed rather than inviting a fresh install", () => {
    const skills = renderPickerBody(
      sessionDraft(),
      "skills",
      failRead(seededClient(), pluginsKey),
    );
    expect(skills.getByTestId("skills-read-error").textContent).toContain(
      "Couldn’t load skill bundles",
    );
    expect(skills.queryByText(/Install skill bundles in Settings/)).toBeNull();

    const memory = renderPickerBody(
      sessionDraft(),
      "memory",
      failRead(seededClient(), memorySpacesKey),
    );
    expect(memory.getByTestId("memory-read-error")).not.toBeNull();
    expect(memory.queryByText(/Create a memory space first/)).toBeNull();
  });

  // The amber dot means "look at this", and a dead config read is exactly
  // that — but a healthy server with a model chosen must not wear one.
  it("marks the config-fed keys as needing attention only when the read failed", () => {
    const failed = pickers(sessionDraft(), failRead(seededClient(), settingsKey));
    expect(failed.find((p) => p.key === "model")?.warn).toBe(true);
    expect(failed.find((p) => p.key === "environment")?.warn).toBe(true);

    const healthy = pickers(sessionDraft(), seededClient());
    expect(healthy.find((p) => p.key === "model")?.warn).toBe(false);
    expect(healthy.find((p) => p.key === "environment")?.warn).toBe(false);
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
