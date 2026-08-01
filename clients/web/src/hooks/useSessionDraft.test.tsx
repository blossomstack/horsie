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
  return client;
}

function render(client: QueryClient) {
  return renderHook(() => useSessionDraft(), {
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
