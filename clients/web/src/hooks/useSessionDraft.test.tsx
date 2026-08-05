import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import type {
  GitHubStatus,
  MemorySpaceView,
  McpServerView,
  PluginView,
  SettingsView,
} from "../api/types";
import { DRAFT_STORAGE_KEY, type DraftPayload } from "./draftPersistence";
import { githubKeys } from "./useGithub";
import { memorySpacesKey } from "./useMemory";
import { mcpKeys } from "./useMcp";
import { pluginsKey } from "./usePlugins";
import { useSessionDraft } from "./useSessionDraft";
import { settingsKey } from "./useSettings";
import { workflowKeys } from "./useWorkflows";

const settings: SettingsView = {
  providers: [],
  models: [
    { alias: "sonnet", provider: "p", modelId: "m1" },
    { alias: "opus", provider: "p", modelId: "m2" },
  ],
  vendors: [
    {
      name: "local",
      isDefault: true,
      capabilities: { supportsProvisioning: false },
    },
    {
      name: "velos",
      isDefault: false,
      capabilities: { supportsProvisioning: true },
    },
  ],
  defaultVendor: "local",
  info: {
    configPath: "",
    database: "",
    stateDir: "",
    dataDir: "",
    pluginsDir: "",
    version: "0",
    journalBackend: "file",
  },
  restartRequired: false,
};

const bundles: PluginView[] = [
  {
    name: "bundle-a",
    sourceUrl: "",
    skillCount: 1,
    hasHooks: false,
    enabledDefault: true,
    artifactSize: 0,
  },
  {
    name: "bundle-b",
    sourceUrl: "",
    skillCount: 1,
    hasHooks: false,
    enabledDefault: false,
    artifactSize: 0,
  },
];

const mcpServers: McpServerView[] = [
  { name: "mcp-x", url: "http://x", enabled: true, auth: { kind: "None", value: {} } },
];

const memorySpaces: MemorySpaceView[] = [{ name: "horsie", description: "", memoryCount: 0 }];

const ghStatus: GitHubStatus = { connected: false, appConfigured: false, repoCount: 0 };

function makeClient(): QueryClient {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  client.setQueryData(pluginsKey, bundles);
  client.setQueryData(mcpKeys.servers, mcpServers);
  client.setQueryData(memorySpacesKey, memorySpaces);
  client.setQueryData(githubKeys.status, ghStatus);
  client.setQueryData(workflowKeys.all, [
    { name: "triage", description: "", start: "a", steps: [], createdAt: "0", updatedAt: "0" },
  ]);
  return client;
}

function render(client: QueryClient, workflow?: string) {
  return renderHook(() => useSessionDraft(workflow), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    ),
  });
}

function storeDraft(draft: Partial<DraftPayload>) {
  const full: DraftPayload = {
    v: 1,
    vendor: "",
    model: "",
    repos: {},
    skills: [],
    mcp: [],
    memorySpaces: [],
    ...draft,
      thinkingEffort: "",
};
  localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(full));
}

beforeEach(() => localStorage.clear());

describe("useSessionDraft persistence", () => {
  it("first visit seeds server defaults and default-enabled bundles", async () => {
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    expect(result.current.vendor).toBe("local");
    await waitFor(() => expect([...result.current.skills]).toEqual(["bundle-a"]));
  });

  it("restores a stored draft and suppresses bundle seeding", async () => {
    storeDraft({ vendor: "velos", model: "opus", skills: [], mcp: ["mcp-x"] });
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.mcp.has("mcp-x")).toBe(true));
    expect(result.current.model).toBe("opus");
    expect(result.current.vendor).toBe("velos");
    // Stored (deliberately empty) skills selection must NOT be re-seeded.
    expect(result.current.skills.size).toBe(0);
  });

  it("a stored draft equal to the defaults still suppresses seeding", async () => {
    storeDraft({ vendor: "local", model: "sonnet", skills: [] });
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    expect(result.current.skills.size).toBe(0);
  });

  it("falls back to defaults when the stored model/vendor are gone", async () => {
    storeDraft({ vendor: "gone", model: "gone" });
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    expect(result.current.vendor).toBe("local");
  });

  it("filters stored selections that no longer exist", async () => {
    storeDraft({
      skills: ["bundle-a", "gone"],
      mcp: ["mcp-x", "gone"],
      memorySpaces: ["horsie", "gone"],
    });
    const { result } = render(makeClient());
    await waitFor(() => expect([...result.current.skills]).toEqual(["bundle-a"]));
    expect([...result.current.mcp]).toEqual(["mcp-x"]);
    expect([...result.current.memorySpaces]).toEqual(["horsie"]);
  });

  it("persists setter changes to localStorage", async () => {
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    act(() => result.current.setModel("opus"));
    const stored = JSON.parse(localStorage.getItem(DRAFT_STORAGE_KEY)!) as DraftPayload;
    expect(stored.model).toBe("opus");
  });
});

describe("useSessionDraft workflow channel", () => {
  it("starts at none, and preselects the one the Run link named", async () => {
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    expect(result.current.workflow).toBe("");

    const preselected = render(makeClient(), "triage");
    await waitFor(() => expect(preselected.result.current.workflow).toBe("triage"));
  });

  // A `Run` link for a workflow deleted since, or a hand-typed query string.
  it("ignores a preselection that names no workflow", async () => {
    const { result } = render(makeClient(), "gone");
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    expect(result.current.workflow).toBe("");
  });

  // The whole point of not persisting it: this must not come back next visit.
  it("keeps the selection out of the stored draft", async () => {
    const { result } = render(makeClient());
    await waitFor(() => expect(result.current.model).toBe("sonnet"));
    act(() => result.current.setWorkflow("triage"));
    expect(result.current.workflow).toBe("triage");
    const stored = JSON.parse(localStorage.getItem(DRAFT_STORAGE_KEY)!);
    expect(stored.workflow).toBeUndefined();
  });

  // A run's model comes from each step's preset, so the model channel is
  // neither shown nor required.
  it("does not require a model to start a run", async () => {
    storeDraft({ vendor: "local", model: "" });
    const { result } = render(makeClient(), "triage");
    await waitFor(() => expect(result.current.workflow).toBe("triage"));
    expect(result.current.blockedReason).toBeNull();
    expect(result.current.canSend).toBe(true);
  });

  // Dropping the model requirement must not drop the runtime one: a run needs
  // somewhere to run exactly as much as a session does. Reconciliation puts the
  // server default back on any draft, so the only way to have no runtime is for
  // the server to have none connected.
  it("still requires a runtime to start a run", async () => {
    const client = makeClient();
    client.setQueryData(settingsKey, { ...settings, vendors: [], defaultVendor: "" });
    const { result } = render(client, "triage");
    await waitFor(() => expect(result.current.workflow).toBe("triage"));
    expect(result.current.blockedReason).toBe("Select a runtime to start.");
  });

  it("builds a run request carrying the input and the runtime", async () => {
    storeDraft({ vendor: "velos", model: "opus" });
    const { result } = render(makeClient(), "triage");
    await waitFor(() => expect(result.current.vendor).toBe("velos"));
    expect(result.current.buildRunRequest("ship it")).toEqual({
      input: "ship it",
      vendor: "velos",
      repos: undefined,
    });
  });

  it("sends the picked repos with a run on a provisioning runtime", async () => {
    storeDraft({ vendor: "velos", model: "opus", repos: { "o/r": "" } });
    const { result } = render(makeClient(), "triage");
    await waitFor(() => expect(result.current.provisions).toBe(true));
    expect(result.current.buildRunRequest("ship it").repos).toEqual([
      { url: "https://github.com/o/r", gitRef: undefined },
    ]);
  });
});
