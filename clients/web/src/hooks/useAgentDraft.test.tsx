import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import type { AgentView, GitHubStatus, SettingsView } from "../api/types";
import { useAgentDraft } from "./useAgentDraft";
import { githubKeys } from "./useGithub";
import { settingsKey } from "./useSettings";

const settings: SettingsView = {
  providers: [],
  models: [
    {
      alias: "sonnet",
      provider: "p",
      modelId: "m1",
      thinkingEfforts: ["low", "high"],
      thinkingEffort: "low",
    },
    { alias: "haiku", provider: "p", modelId: "m2" },
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

const ghStatus: GitHubStatus = {
  connected: true,
  appConfigured: true,
  repoCount: 1,
};

function render(initial?: AgentView) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  client.setQueryData(githubKeys.status, ghStatus);
  return renderHook(() => useAgentDraft(initial), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    ),
  });
}

const preset: AgentView = {
  name: "reviewer",
  description: "reviews PRs",
  vendor: "velos",
  model: "sonnet",
  repos: [{ url: "https://github.com/org/api", gitRef: "dev" }],
  plugins: ["superpowers"],
  mcpServers: ["mcp-x"],
  memorySpaces: ["horsie"],
  thinkingEffort: "high",
  createdAt: "1",
  updatedAt: "1",
};

describe("useAgentDraft", () => {
  it("starts blank without an initial preset", () => {
    const { result } = render();
    expect(result.current.model).toBe("");
    expect(result.current.vendor).toBe("");
    expect(result.current.repos.size).toBe(0);
    const input = result.current.buildAgentInput("a", "");
    expect(input).toEqual({
      name: "a",
      description: undefined,
      vendor: undefined,
      model: "",
      repos: undefined,
      plugins: undefined,
      mcpServers: undefined,
      memorySpaces: undefined,
      thinkingEffort: undefined,
    });
  });

  it("populates every picker from the preset, mapping repo urls back to full names", () => {
    const { result } = render(preset);
    expect(result.current.vendor).toBe("velos");
    expect(result.current.model).toBe("sonnet");
    expect(result.current.repos.get("org/api")).toBe("dev");
    expect([...result.current.skills]).toEqual(["superpowers"]);
    expect([...result.current.mcp]).toEqual(["mcp-x"]);
    expect([...result.current.memorySpaces]).toEqual(["horsie"]);
    expect(result.current.thinkingEffort).toBe("high");
    expect(result.current.provisions).toBe(true);
  });

  it("round-trips the preset through buildAgentInput", () => {
    const { result } = render(preset);
    expect(result.current.buildAgentInput("reviewer", "reviews PRs")).toEqual({
      name: "reviewer",
      description: "reviews PRs",
      vendor: "velos",
      model: "sonnet",
      repos: [{ url: "https://github.com/org/api", gitRef: "dev" }],
      plugins: ["superpowers"],
      mcpServers: ["mcp-x"],
      memorySpaces: ["horsie"],
      thinkingEffort: "high",
    });
  });

  it("drops repos/skills/mcp when the vendor cannot provision, but keeps memory", () => {
    const { result } = render(preset);
    act(() => result.current.setVendor("local"));
    const input = result.current.buildAgentInput("reviewer", "");
    expect(input.repos).toBeUndefined();
    expect(input.plugins).toBeUndefined();
    expect(input.mcpServers).toBeUndefined();
    expect(input.memorySpaces).toEqual(["horsie"]);
  });

  it("falls back to the model default when the effort is not offered", () => {
    const { result } = render(preset);
    act(() => result.current.setModel("haiku"));
    expect(result.current.thinkingEfforts).toEqual([]);
    expect(result.current.thinkingEffort).toBe("");
    expect(
      result.current.buildAgentInput("reviewer", "").thinkingEffort,
    ).toBeUndefined();
  });
});
