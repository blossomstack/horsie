# Session Draft Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist the web UI's new-session draft (runtime, model, repos, skills, MCP servers, memory spaces) to localStorage so it survives reloads, with stale entries silently reconciled against server data.

**Architecture:** A generic `usePersistentState<T>` hook mirrors state to localStorage. `useSessionDraft` holds the whole draft as one versioned JSON payload in that hook; pure functions in `draftPersistence.ts` handle (de)serialization and reconciliation. Unit tests run on a newly introduced Vitest setup; one Playwright e2e test covers the real reload flow.

**Tech Stack:** React 19, TypeScript, Vite 8, bun, Vitest + @testing-library/react + jsdom (new), Playwright (existing e2e).

**Spec:** `docs/superpowers/specs/2026-07-27-session-draft-persistence-design.md`

## Global Constraints

- All work happens in the worktree `.horsie/worktrees/session-draft-persistence`, branch `feat/session-draft-persistence`.
- Web client code lives in `clients/web`; package manager is **bun** (`bun install`, `bun run ...`); `bun.lock` is committed.
- TypeScript is strict with `noUnusedLocals`/`noUnusedParameters`; `bun run build` (= `tsc -b && vite build`) must stay green.
- Stored payload is a single versioned JSON object under localStorage key `horsie-session-draft`, `{ v: 1, ... }`. Unknown versions and corrupt JSON are discarded silently.
- The `SessionDraft` public interface (fields + setters) and `buildRequest()` must remain unchanged — `SessionConfigBar.tsx` and friends are not touched.
- No server, protocol (`.fl`), or generated-type changes.

---

### Task 1: Vitest tooling + `draftPersistence` pure module

**Files:**
- Modify: `clients/web/package.json` (add script + devDependencies)
- Modify: `clients/web/vite.config.ts` (add vitest config block)
- Create: `clients/web/src/hooks/draftPersistence.ts`
- Test: `clients/web/src/hooks/draftPersistence.test.ts`

**Interfaces:**
- Consumes: nothing (first task).
- Produces (used by Tasks 2-4):
  - `DRAFT_STORAGE_KEY: string` (= `"horsie-session-draft"`)
  - `interface DraftPayload { v: 1; vendor: string; model: string; repos: Record<string, string>; skills: string[]; mcp: string[]; memorySpaces: string[] }`
  - `emptyDraft(): DraftPayload`
  - `parseDraftPayload(raw: unknown): DraftPayload | undefined`
  - `loadDraftPayload(storage?: Storage): DraftPayload | undefined`
  - `reconcileModelVendor(draft: DraftPayload, modelAliases: readonly string[], activeVendorNames: readonly string[], defaultVendor: string): DraftPayload` — returns the **same reference** when nothing changes
  - `filterSkills(draft: DraftPayload, installed: ReadonlySet<string>): DraftPayload` — same-ref when unchanged
  - `filterMcpServers(draft: DraftPayload, enabled: ReadonlySet<string>): DraftPayload` — same-ref when unchanged
  - `filterMemorySpaces(draft: DraftPayload, existing: ReadonlySet<string>): DraftPayload` — same-ref when unchanged

- [ ] **Step 1: Install Vitest tooling and add the script**

```bash
cd clients/web
bun add -d vitest jsdom @testing-library/react @testing-library/dom
```

In `clients/web/package.json`, add to `"scripts"` (keep alphabetical-ish placement next to the other test script):

```json
"test:unit": "vitest run",
```

- [ ] **Step 2: Add the Vitest block to `vite.config.ts`**

Change the import and add the `test` key (the rest of the file is unchanged):

```ts
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// ... existing HORSIE_SERVER comment + const unchanged ...

export default defineConfig({
  plugins: [react(), tailwindcss()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
  server: {
    // ... unchanged ...
  },
});
```

- [ ] **Step 3: Write the failing tests**

Create `clients/web/src/hooks/draftPersistence.test.ts`:

```ts
import { beforeEach, describe, expect, it } from "vitest";
import {
  DRAFT_STORAGE_KEY,
  emptyDraft,
  filterMcpServers,
  filterMemorySpaces,
  filterSkills,
  loadDraftPayload,
  parseDraftPayload,
  reconcileModelVendor,
  type DraftPayload,
} from "./draftPersistence";

const sample: DraftPayload = {
  v: 1,
  vendor: "velos",
  model: "sonnet",
  repos: { "owner/repo": "" },
  skills: ["bundle-a"],
  mcp: ["mcp-x"],
  memorySpaces: ["horsie"],
};

beforeEach(() => localStorage.clear());

describe("parseDraftPayload", () => {
  it("accepts a well-formed payload", () => {
    expect(parseDraftPayload(sample)).toEqual(sample);
  });

  it("rejects a wrong version", () => {
    expect(parseDraftPayload({ ...sample, v: 2 })).toBeUndefined();
  });

  it("rejects non-objects and missing fields", () => {
    expect(parseDraftPayload(null)).toBeUndefined();
    expect(parseDraftPayload("nope")).toBeUndefined();
    const noModel: Record<string, unknown> = { ...sample };
    delete noModel.model;
    expect(parseDraftPayload(noModel)).toBeUndefined();
  });

  it("rejects wrongly-typed fields", () => {
    expect(parseDraftPayload({ ...sample, skills: "bundle-a" })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, skills: [1] })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, repos: ["owner/repo"] })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, vendor: 42 })).toBeUndefined();
  });
});

describe("loadDraftPayload", () => {
  it("returns undefined when the key is absent", () => {
    expect(loadDraftPayload()).toBeUndefined();
  });

  it("returns undefined for corrupt JSON", () => {
    localStorage.setItem(DRAFT_STORAGE_KEY, "{not json");
    expect(loadDraftPayload()).toBeUndefined();
  });

  it("round-trips a stored payload", () => {
    localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(sample));
    expect(loadDraftPayload()).toEqual(sample);
  });
});

describe("reconcileModelVendor", () => {
  const aliases = ["sonnet", "opus"];
  const vendors = ["local", "velos"];

  it("returns the same reference when both are still valid", () => {
    expect(reconcileModelVendor(sample, aliases, vendors, "local")).toBe(sample);
  });

  it("falls back to the first model when the stored one is gone", () => {
    const next = reconcileModelVendor({ ...sample, model: "gone" }, aliases, vendors, "local");
    expect(next.model).toBe("sonnet");
  });

  it("falls back to the default vendor when the stored one is gone or inactive", () => {
    const next = reconcileModelVendor({ ...sample, vendor: "gone" }, aliases, vendors, "local");
    expect(next.vendor).toBe("local");
  });

  it("clears the model when no models are configured", () => {
    const next = reconcileModelVendor(sample, [], vendors, "local");
    expect(next.model).toBe("");
  });
});

describe("selection filters", () => {
  it("filterSkills drops bundles that are no longer installed", () => {
    const next = filterSkills({ ...sample, skills: ["bundle-a", "gone"] }, new Set(["bundle-a"]));
    expect(next.skills).toEqual(["bundle-a"]);
  });

  it("filterMcpServers drops servers that are no longer enabled", () => {
    const next = filterMcpServers({ ...sample, mcp: ["mcp-x", "gone"] }, new Set(["mcp-x"]));
    expect(next.mcp).toEqual(["mcp-x"]);
  });

  it("filterMemorySpaces drops spaces that no longer exist", () => {
    const next = filterMemorySpaces(
      { ...sample, memorySpaces: ["horsie", "gone"] },
      new Set(["horsie"]),
    );
    expect(next.memorySpaces).toEqual(["horsie"]);
  });

  it("each filter returns the same reference when nothing is stale", () => {
    expect(filterSkills(sample, new Set(["bundle-a"]))).toBe(sample);
    expect(filterMcpServers(sample, new Set(["mcp-x"]))).toBe(sample);
    expect(filterMemorySpaces(sample, new Set(["horsie"]))).toBe(sample);
  });
});

describe("emptyDraft", () => {
  it("is all-empty with version 1", () => {
    expect(emptyDraft()).toEqual({
      v: 1,
      vendor: "",
      model: "",
      repos: {},
      skills: [],
      mcp: [],
      memorySpaces: [],
    });
  });
});
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cd clients/web && bun run test:unit`
Expected: FAIL — module `./draftPersistence` does not exist.

- [ ] **Step 5: Implement `draftPersistence.ts`**

Create `clients/web/src/hooks/draftPersistence.ts`:

```ts
// Pure (de)serialization + reconciliation for the localStorage-persisted
// new-session draft. Kept free of React so every rule here is unit-testable;
// `useSessionDraft` wires these into hooks.

export const DRAFT_STORAGE_KEY = "horsie-session-draft";

/** The stored draft, v1. Plain JSON types only — never Map/Set. */
export interface DraftPayload {
  v: 1;
  vendor: string;
  model: string;
  /** fullName → gitRef ("" = default branch). */
  repos: Record<string, string>;
  skills: string[];
  mcp: string[];
  memorySpaces: string[];
}

export function emptyDraft(): DraftPayload {
  return { v: 1, vendor: "", model: "", repos: {}, skills: [], mcp: [], memorySpaces: [] };
}

function isStringArray(x: unknown): x is string[] {
  return Array.isArray(x) && x.every((i) => typeof i === "string");
}

function isStringRecord(x: unknown): x is Record<string, string> {
  return (
    typeof x === "object" &&
    x !== null &&
    !Array.isArray(x) &&
    Object.values(x).every((v) => typeof v === "string")
  );
}

/**
 * Validate an already-parsed JSON value. Returns `undefined` for anything
 * that isn't a v1 payload — wrong version, missing or mistyped fields — so
 * callers fall back to first-visit behavior instead of trusting bad data.
 */
export function parseDraftPayload(raw: unknown): DraftPayload | undefined {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return undefined;
  const p = raw as Record<string, unknown>;
  if (p.v !== 1) return undefined;
  if (typeof p.vendor !== "string" || typeof p.model !== "string") return undefined;
  if (!isStringRecord(p.repos)) return undefined;
  if (!isStringArray(p.skills) || !isStringArray(p.mcp) || !isStringArray(p.memorySpaces))
    return undefined;
  return {
    v: 1,
    vendor: p.vendor,
    model: p.model,
    repos: p.repos,
    skills: p.skills,
    mcp: p.mcp,
    memorySpaces: p.memorySpaces,
  };
}

/**
 * Read and validate the stored draft. `undefined` means "no usable stored
 * draft" (absent, corrupt, or unknown version) — the signal that decides
 * whether default-enabled bundles get seeded, so it must not be derived
 * from value comparison.
 */
export function loadDraftPayload(storage: Storage = localStorage): DraftPayload | undefined {
  try {
    const rawJson = storage.getItem(DRAFT_STORAGE_KEY);
    if (rawJson === null) return undefined;
    return parseDraftPayload(JSON.parse(rawJson));
  } catch {
    return undefined;
  }
}

/**
 * Keep model/vendor only while they still exist server-side; otherwise fall
 * back to the first model / the server's default vendor. Returns the same
 * reference when nothing changed so effects can skip redundant writes.
 */
export function reconcileModelVendor(
  draft: DraftPayload,
  modelAliases: readonly string[],
  activeVendorNames: readonly string[],
  defaultVendor: string,
): DraftPayload {
  const model = modelAliases.includes(draft.model) ? draft.model : (modelAliases[0] ?? "");
  const vendor = activeVendorNames.includes(draft.vendor) ? draft.vendor : defaultVendor;
  if (model === draft.model && vendor === draft.vendor) return draft;
  return { ...draft, model, vendor };
}

function filterField(
  draft: DraftPayload,
  field: "skills" | "mcp" | "memorySpaces",
  keep: ReadonlySet<string>,
): DraftPayload {
  const filtered = draft[field].filter((name) => keep.has(name));
  if (filtered.length === draft[field].length) return draft;
  return { ...draft, [field]: filtered };
}

/** Drop selected bundles that are no longer installed. Same ref if unchanged. */
export function filterSkills(draft: DraftPayload, installed: ReadonlySet<string>): DraftPayload {
  return filterField(draft, "skills", installed);
}

/** Drop selected MCP servers that are no longer enabled. Same ref if unchanged. */
export function filterMcpServers(draft: DraftPayload, enabled: ReadonlySet<string>): DraftPayload {
  return filterField(draft, "mcp", enabled);
}

/** Drop selected memory spaces that no longer exist. Same ref if unchanged. */
export function filterMemorySpaces(
  draft: DraftPayload,
  existing: ReadonlySet<string>,
): DraftPayload {
  return filterField(draft, "memorySpaces", existing);
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd clients/web && bun run test:unit`
Expected: PASS (all `draftPersistence` tests).

- [ ] **Step 7: Verify typecheck still passes**

Run: `cd clients/web && bun run build`
Expected: builds cleanly (`tsc -b` covers the new files).

- [ ] **Step 8: Commit**

```bash
cd clients/web
git add package.json bun.lock vite.config.ts src/hooks/draftPersistence.ts src/hooks/draftPersistence.test.ts
git commit -m "feat(web): vitest setup + draft persistence pure module"
```

---

### Task 2: `usePersistentState` hook

**Files:**
- Create: `clients/web/src/hooks/usePersistentState.ts`
- Test: `clients/web/src/hooks/usePersistentState.test.ts`

**Interfaces:**
- Consumes: nothing from Task 1 (generic hook; `parseDraftPayload` is only plugged in at Task 3).
- Produces (used by Task 3):
  - `usePersistentState<T>(key: string, initial: T, options?: { serialize?: (value: T) => unknown; deserialize?: (raw: unknown) => T | undefined }): [T, (next: T) => void]`

- [ ] **Step 1: Write the failing tests**

Create `clients/web/src/hooks/usePersistentState.test.ts`:

```ts
import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { usePersistentState } from "./usePersistentState";

const KEY = "test-persistent-state";

beforeEach(() => localStorage.clear());

describe("usePersistentState", () => {
  it("returns the initial value when the key is absent", () => {
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("hello");
  });

  it("hydrates from an existing stored JSON value", () => {
    localStorage.setItem(KEY, JSON.stringify("stored"));
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("stored");
  });

  it("writes through to localStorage on set", () => {
    const { result } = renderHook(() => usePersistentState(KEY, 0));
    act(() => result.current[1](42));
    expect(result.current[0]).toBe(42);
    expect(JSON.parse(localStorage.getItem(KEY)!)).toBe(42);
  });

  it("falls back to the initial value on corrupt JSON", () => {
    localStorage.setItem(KEY, "{not json");
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("hello");
  });

  it("falls back to the initial value when deserialize returns undefined", () => {
    localStorage.setItem(KEY, JSON.stringify({ v: 99 }));
    const { result } = renderHook(() =>
      usePersistentState(KEY, { v: 1 }, {
        deserialize: (raw) => {
          const p = raw as { v?: number };
          return p.v === 1 ? { v: 1 } : undefined;
        },
      }),
    );
    expect(result.current[0]).toEqual({ v: 1 });
  });

  it("round-trips a Set through custom serializers", () => {
    const { result } = renderHook(() =>
      usePersistentState<Set<string>>(KEY, new Set(), {
        serialize: (s) => [...s],
        deserialize: (raw) => (Array.isArray(raw) ? new Set(raw as string[]) : undefined),
      }),
    );
    act(() => result.current[1](new Set(["a", "b"])));
    expect(JSON.parse(localStorage.getItem(KEY)!)).toEqual(["a", "b"]);

    const again = renderHook(() =>
      usePersistentState<Set<string>>(KEY, new Set(), {
        serialize: (s) => [...s],
        deserialize: (raw) => (Array.isArray(raw) ? new Set(raw as string[]) : undefined),
      }),
    );
    expect([...again.result.current[0]].sort()).toEqual(["a", "b"]);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd clients/web && bun run test:unit`
Expected: FAIL — module `./usePersistentState` does not exist.

- [ ] **Step 3: Implement `usePersistentState.ts`**

Create `clients/web/src/hooks/usePersistentState.ts`:

```ts
import { useState } from "react";

export interface PersistentStateOptions<T> {
  /** Map the value to a JSON-serializable shape (default: identity). */
  serialize?: (value: T) => unknown;
  /**
   * Validate a parsed JSON value; return `undefined` to reject it (wrong
   * version, bad shape) and fall back to `initial`.
   */
  deserialize?: (raw: unknown) => T | undefined;
}

/**
 * `useState` mirrored to localStorage under `key`. Hydrates lazily on first
 * render; every set writes through. Missing key, corrupt JSON, or a rejected
 * deserialize all fall back to `initial`. No cross-tab sync — last write
 * wins, same as `useUiSettings`.
 */
export function usePersistentState<T>(
  key: string,
  initial: T,
  options: PersistentStateOptions<T> = {},
): [T, (next: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      const rawJson = localStorage.getItem(key);
      if (rawJson === null) return initial;
      const parsed: unknown = JSON.parse(rawJson);
      const hydrated = options.deserialize ? options.deserialize(parsed) : (parsed as T);
      return hydrated === undefined ? initial : hydrated;
    } catch {
      return initial;
    }
  });

  const set = (next: T) => {
    setValue(next);
    try {
      localStorage.setItem(
        key,
        JSON.stringify(options.serialize ? options.serialize(next) : next),
      );
    } catch {
      // Storage full or unavailable — keep the in-memory state; a lost
      // preference must never break the UI.
    }
  };

  return [value, set];
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd clients/web && bun run test:unit`
Expected: PASS (Tasks 1 + 2 tests).

- [ ] **Step 5: Commit**

```bash
cd clients/web
git add src/hooks/usePersistentState.ts src/hooks/usePersistentState.test.ts
git commit -m "feat(web): usePersistentState hook mirroring state to localStorage"
```

---

### Task 3: Rewire `useSessionDraft` onto the persisted payload

**Files:**
- Modify: `clients/web/src/hooks/useSessionDraft.ts` (full rewrite below)
- Test: `clients/web/src/hooks/useSessionDraft.test.tsx`

**Interfaces:**
- Consumes (from Tasks 1-2): `DRAFT_STORAGE_KEY`, `DraftPayload`, `emptyDraft`, `parseDraftPayload`, `loadDraftPayload`, `reconcileModelVendor`, `filterSkills`, `filterMcpServers`, `filterMemorySpaces`, `usePersistentState`.
- Consumes (existing hooks, no changes to them): `useSettings` (`settingsKey`), `useGithubStatus` (`githubKeys.status`), `usePlugins` (`pluginsKey`), `useMcpServers` (`mcpKeys.servers`), `useMemorySpaces` (`memorySpacesKey`).
- Produces: the **unchanged** `SessionDraft` interface — `vendor`, `model` (strings), `repos` (`Map<string,string>`), `skills`, `mcp`, `memorySpaces` (`Set<string>`), their setters, `provisions`, `githubConnected`, `canSend`, `blockedReason`, `buildRequest()`. `SessionConfigBar.tsx` must compile unmodified.

- [ ] **Step 1: Write the failing tests**

Create `clients/web/src/hooks/useSessionDraft.test.tsx`:

```tsx
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
    { name: "local", active: true, isDefault: true },
    { name: "velos", active: true, isDefault: false },
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd clients/web && bun run test:unit`
Expected: FAIL — the restored/first-visit assertions fail because the draft is not yet persisted (e.g. "restores a stored draft" gets `model: "sonnet"` instead of `"opus"`).

- [ ] **Step 3: Rewrite `useSessionDraft.ts`**

Replace the entire contents of `clients/web/src/hooks/useSessionDraft.ts` with:

```ts
import { useEffect, useMemo, useState } from "react";
import type { CreateSessionRequest, RepoConfig } from "../api/types";
import {
  DRAFT_STORAGE_KEY,
  emptyDraft,
  filterMcpServers,
  filterMemorySpaces,
  filterSkills,
  loadDraftPayload,
  parseDraftPayload,
  reconcileModelVendor,
  type DraftPayload,
} from "./draftPersistence";
import { useGithubStatus } from "./useGithub";
import { useMemorySpaces } from "./useMemory";
import { useMcpServers } from "./useMcp";
import { usePersistentState } from "./usePersistentState";
import { usePlugins } from "./usePlugins";
import { useSettings } from "./useSettings";

export interface SessionDraft {
  vendor: string;
  setVendor: (v: string) => void;
  model: string;
  setModel: (m: string) => void;
  /** fullName → gitRef ("" = default branch). */
  repos: Map<string, string>;
  setRepos: (m: Map<string, string>) => void;
  skills: Set<string>;
  setSkills: (s: Set<string>) => void;
  mcp: Set<string>;
  setMcp: (s: Set<string>) => void;
  /** Memory spaces the session may read and write. */
  memorySpaces: Set<string>;
  setMemorySpaces: (s: Set<string>) => void;
  provisions: boolean;
  githubConnected: boolean;
  canSend: boolean;
  blockedReason: string | null;
  buildRequest: () => CreateSessionRequest;
}

export function useSessionDraft(): SessionDraft {
  const { data: settings } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const { data: bundles } = usePlugins();
  const { data: mcpServers } = useMcpServers();
  const { data: memorySpaces } = useMemorySpaces();
  const models = settings?.models ?? [];
  const activeVendors = useMemo(
    () => (settings?.vendors ?? []).filter((v) => v.active),
    [settings],
  );

  // Load-once snapshot: `undefined` means this browser has no usable stored
  // draft (first visit, corrupt payload, unknown version) — the signal that
  // decides whether default-enabled bundles get seeded below.
  const [storedAtMount] = useState(() => loadDraftPayload());
  const [draft, setDraft] = usePersistentState<DraftPayload>(
    DRAFT_STORAGE_KEY,
    storedAtMount ?? emptyDraft(),
    { deserialize: parseDraftPayload },
  );

  // Keep model/vendor on still-existing choices as server config changes.
  useEffect(() => {
    if (!settings) return;
    const next = reconcileModelVendor(
      draft,
      models.map((m) => m.alias),
      activeVendors.map((v) => v.name),
      settings.defaultVendor,
    );
    if (next !== draft) setDraft(next);
  }, [settings, models, activeVendors, draft]);

  // First visit only: pre-select the server's default-enabled bundles. A
  // stored draft (even one equal to the defaults, even with empty skills)
  // suppresses seeding — the user's last choice wins.
  const [skillsSeeded, setSkillsSeeded] = useState(storedAtMount !== undefined);
  useEffect(() => {
    if (skillsSeeded || !bundles) return;
    setDraft({
      ...draft,
      skills: bundles.filter((b) => b.enabledDefault).map((b) => b.name),
    });
    setSkillsSeeded(true);
  }, [bundles, skillsSeeded, draft]);

  // A restored draft may name bundles/servers/spaces that no longer exist —
  // drop those once the authoritative lists arrive (one pass, silently).
  const [staleFiltered, setStaleFiltered] = useState(false);
  useEffect(() => {
    if (staleFiltered || !bundles || !mcpServers || !memorySpaces) return;
    const next = filterMemorySpaces(
      filterMcpServers(
        filterSkills(draft, new Set(bundles.map((b) => b.name))),
        new Set(mcpServers.filter((s) => s.enabled).map((s) => s.name)),
      ),
      new Set(memorySpaces.map((sp) => sp.name)),
    );
    if (next !== draft) setDraft(next);
    setStaleFiltered(true);
  }, [staleFiltered, bundles, mcpServers, memorySpaces, draft]);

  const selectedVendor = activeVendors.find(
    (v) => v.name === (draft.vendor || settings?.defaultVendor),
  );
  const provisions = !!selectedVendor?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  const blockedReason = useMemo(() => {
    if (!draft.model.trim()) return "Select a model to start.";
    if (!draft.vendor.trim()) return "Select a runtime to start.";
    if (provisions && !githubConnected)
      return "Connect GitHub to use this runtime.";
    return null;
  }, [draft.model, draft.vendor, provisions, githubConnected]);

  const buildRequest = (): CreateSessionRequest => {
    const repoList: RepoConfig[] = provisions
      ? Object.entries(draft.repos).map(([fullName, ref]) => ({
          url: `https://github.com/${fullName}`,
          gitRef: ref.trim() || undefined,
        }))
      : [];
    return {
      agent: {
        model: draft.model.trim(),
        usePlugins: provisions ? true : undefined,
        mcpServers: provisions && draft.mcp.length ? draft.mcp : undefined,
        // Not gated on `provisions`: memories are served by the server itself,
        // so they work on every vendor, including ones that can't provision.
        memorySpaces: draft.memorySpaces.length ? draft.memorySpaces : undefined,
      },
      vendor: draft.vendor.trim() || undefined,
      repos: repoList.length ? repoList : undefined,
      plugins: provisions && draft.skills.length ? draft.skills : undefined,
    };
  };

  return {
    vendor: draft.vendor,
    setVendor: (vendor) => setDraft({ ...draft, vendor }),
    model: draft.model,
    setModel: (model) => setDraft({ ...draft, model }),
    repos: new Map(Object.entries(draft.repos)),
    setRepos: (repos) => setDraft({ ...draft, repos: Object.fromEntries(repos) }),
    skills: new Set(draft.skills),
    setSkills: (skills) => setDraft({ ...draft, skills: [...skills] }),
    mcp: new Set(draft.mcp),
    setMcp: (mcp) => setDraft({ ...draft, mcp: [...mcp] }),
    memorySpaces: new Set(draft.memorySpaces),
    setMemorySpaces: (memorySpaces) => setDraft({ ...draft, memorySpaces: [...memorySpaces] }),
    provisions,
    githubConnected,
    canSend: blockedReason === null,
    blockedReason,
    buildRequest,
  };
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd clients/web && bun run test:unit`
Expected: PASS (all tasks' tests).

- [ ] **Step 5: Verify typecheck/build**

Run: `cd clients/web && bun run build`
Expected: builds cleanly — in particular `SessionConfigBar.tsx` compiles unmodified against the unchanged `SessionDraft` interface.

- [ ] **Step 6: Commit**

```bash
cd clients/web
git add src/hooks/useSessionDraft.ts src/hooks/useSessionDraft.test.tsx
git commit -m "feat(web): persist new-session draft to localStorage"
```

---

### Task 4: Playwright e2e — reload restores the draft

**Files:**
- Test: `clients/web/e2e/m-draft-persistence.spec.ts`

**Interfaces:**
- Consumes: existing e2e `fixtures` (`test`, `expect`, `appBase`, `mock`) and the seeded harness state — two models (`mock-sonnet` default, `openai-mock`) and one vendor (`e2e`, non-provisioning). No harness changes needed.
- Produces: nothing consumed by later code tasks.

- [ ] **Step 1: Write the e2e spec**

Create `clients/web/e2e/m-draft-persistence.spec.ts`:

```ts
// Group M — the new-session draft persists to localStorage and is restored
// after a reload. The e2e vendor is non-provisioning, so only the runtime and
// model chips are exercised here; selection-set restore is unit-tested.
import { expect, test } from "./fixtures";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("M1: model selection survives a page reload", async ({ page, appBase }) => {
  await page.goto(appBase);
  await expect(page.getByTestId("config-model")).toBeVisible();

  // Default is the first model; switch to the other seeded one.
  await expect(page.getByTestId("config-model")).toContainText("mock-sonnet");
  await page.getByTestId("config-model").click();
  await page.locator('[data-testid="model-option"][data-value="openai-mock"]').click();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");

  await page.reload();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");
});

test("M2: clearing the stored draft restores server defaults", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("config-model").click();
  await page.locator('[data-testid="model-option"][data-value="openai-mock"]').click();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");

  await page.evaluate(() => localStorage.removeItem("horsie-session-draft"));
  await page.reload();
  await expect(page.getByTestId("config-model")).toContainText("mock-sonnet");
});
```

- [ ] **Step 2: Build the e2e binaries and run the new spec**

The harness needs the three Rust binaries + web `dist/` built from this worktree:

```bash
cargo build -p horsie-server -p horsie-runtime -p horsie-mock-llm
cd clients/web && bun run build && bunx playwright install chromium
cd clients/web && HORSIE_E2E_SKIP_BUILD=1 bunx playwright test m-draft-persistence
```

Expected: 2 passed. (If the binaries/`dist` are already current, the first two commands can be skipped — `HORSIE_E2E_SKIP_BUILD=1` asserts they exist.)

- [ ] **Step 3: Commit**

```bash
cd clients/web
git add e2e/m-draft-persistence.spec.ts
git commit -m "test(web): e2e for session-draft persistence across reload"
```

---

### Task 5: CI job + full verification

**Files:**
- Modify: `.github/workflows/ci.yml` (add a `web-unit` job)

**Interfaces:**
- Consumes: `bun run test:unit` script from Task 1.
- Produces: CI coverage for unit tests; no code interfaces.

- [ ] **Step 1: Add the `web-unit` job**

In `.github/workflows/ci.yml`, insert this job between `ts-types` and `web-e2e` (mirroring web-e2e's Bun install steps, minus Rust and Playwright):

```yaml
  web-unit:
    name: Web unit tests (Vitest)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0 # v7.0.0

      - name: Install Bun
        run: |
          curl -fsSL https://bun.sh/install | bash
          echo "$HOME/.bun/bin" >> "$GITHUB_PATH"

      - name: Install web dependencies
        working-directory: clients/web
        run: bun install --frozen-lockfile

      - name: Run unit tests
        working-directory: clients/web
        run: bun run test:unit
```

- [ ] **Step 2: Run the full local verification suite**

```bash
cd clients/web && bun run build
cd clients/web && bun run test:unit
cd clients/web && HORSIE_E2E_SKIP_BUILD=1 bunx playwright test
```

Expected: build clean; all unit tests pass; the **whole** e2e suite (groups A–M) passes — in particular J1/J2 (config bar) are unaffected by the `useSessionDraft` rewrite.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run web unit tests (vitest)"
```
