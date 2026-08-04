import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import type { SettingsView } from "../api/types";
import { settingsKey } from "../hooks/useSettings";
import type { ConfigDraft, RuntimeChannel } from "../hooks/useSessionDraft";
import { useConfigPickers } from "./configPickers";

// The default vendor cannot provision — the shape of a `horsie connect` setup,
// which is the case that used to leave an agent preset with no Skills or MCP
// picker at all.
const settings: SettingsView = {
  providers: [],
  models: [{ alias: "sonnet", provider: "p", modelId: "m1" }],
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
    journalBackend: "file",
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
});
