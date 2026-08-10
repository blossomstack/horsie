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
  model: "sonnet",
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
    const input = result.current.buildAgentInput("a", "", "");
    expect(input).toEqual({
      name: "a",
      description: undefined,
      model: "",
      plugins: undefined,
      mcpServers: undefined,
      memorySpaces: undefined,
      thinkingEffort: undefined,
    });
  });

  it("populates every picker from the preset", () => {
    const { result } = render(preset);
    expect(result.current.model).toBe("sonnet");
    expect([...result.current.skills]).toEqual(["superpowers"]);
    expect([...result.current.mcp]).toEqual(["mcp-x"]);
    expect([...result.current.memorySpaces]).toEqual(["horsie"]);
    expect(result.current.thinkingEffort).toBe("high");
  });

  // A preset is agent configuration. Where the work runs and what it runs
  // against are the invocation's, so nothing here carries either.
  it("has no environment channel at all", () => {
    const { result } = render(preset);
    expect("environment" in result.current).toBe(false);
    expect("setEnvironment" in result.current).toBe(false);
  });

  it("round-trips the preset through buildAgentInput", () => {
    const { result } = render(preset);
    expect(result.current.buildAgentInput("reviewer", "reviews PRs", "")).toEqual({
      name: "reviewer",
      description: "reviews PRs",
      model: "sonnet",
      plugins: ["superpowers"],
      mcpServers: ["mcp-x"],
      memorySpaces: ["horsie"],
      thinkingEffort: "high",
    });
  });

  // A preset is saved with `PUT`, which is a full replace: anything the form
  // omits is deleted.
  it("keeps every channel it does carry", () => {
    const { result } = render(preset);
    const input = result.current.buildAgentInput("reviewer", "", "");
    expect(input.plugins).toEqual(["superpowers"]);
    expect(input.mcpServers).toEqual(["mcp-x"]);
    expect(input.memorySpaces).toEqual(["horsie"]);
  });

  it("saves a newly ticked skill and mcp server on a non-provisioning vendor", () => {
    const { result } = render(preset);
    act(() => result.current.setSkills(new Set(["superpowers", "impeccable"])));
    act(() => result.current.setMcp(new Set(["mcp-x", "github"])));
    const input = result.current.buildAgentInput("reviewer", "", "");
    expect(input.plugins).toEqual(["superpowers", "impeccable"]);
    expect(input.mcpServers).toEqual(["mcp-x", "github"]);
  });

  it("falls back to the model default when the effort is not offered", () => {
    const { result } = render(preset);
    act(() => result.current.setModel("haiku"));
    expect(result.current.thinkingEfforts).toEqual([]);
    expect(result.current.thinkingEffort).toBe("");
    expect(
      result.current.buildAgentInput("reviewer", "", "").thinkingEffort,
    ).toBeUndefined();
  });
});
