// Pure (de)serialization, reconciliation and wire mapping for the
// localStorage-persisted new-session draft. Kept free of React so every rule
// here is unit-testable; `useSessionDraft` wires these into hooks.

import type { EnvironmentSpec, RepoConfig } from "../api/types";

export const DRAFT_STORAGE_KEY = "horsie-session-draft";

/**
 * The stored environment: the draft twin of the wire `EnvironmentSpec`.
 *
 * Two shapes rather than a vendor with an optional name, for the same reason
 * the wire union has two: "a predefined environment, and also these repos" is
 * not a thing anyone can mean.
 */
export type EnvironmentDraft =
  | { kind: "runtime"; vendor: string; repos: Record<string, string> }
  | { kind: "named"; name: string };

/** The stored draft, v2. Plain JSON types only — never Map/Set. */
export interface DraftPayload {
  v: 2;
  /** Where the session runs and what it runs against. */
  environment: EnvironmentDraft;
  model: string;
  skills: string[];
  mcp: string[];
  memorySpaces: string[];
  /** Canonical thinking effort; "" = use the model's configured default. */
  thinkingEffort: string;
}

export function emptyDraft(): DraftPayload {
  return {
    v: 2,
    environment: { kind: "runtime", vendor: "", repos: {} },
    model: "",
    skills: [],
    mcp: [],
    memorySpaces: [],
    thinkingEffort: "",
  };
}

function isStringArray(x: unknown): x is string[] {
  return Array.isArray(x) && x.every((i) => typeof i === "string");
}

/** `undefined` for anything that is not one of the two shapes. */
function parseEnvironment(x: unknown): EnvironmentDraft | undefined {
  if (typeof x !== "object" || x === null || Array.isArray(x)) return undefined;
  const e = x as Record<string, unknown>;
  if (e.kind === "named") {
    return typeof e.name === "string" ? { kind: "named", name: e.name } : undefined;
  }
  if (e.kind === "runtime") {
    if (typeof e.vendor !== "string" || !isStringRecord(e.repos)) return undefined;
    return { kind: "runtime", vendor: e.vendor, repos: e.repos };
  }
  return undefined;
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
  // A v1 payload is not migrated. Its `vendor`/`repos` pair is exactly what
  // the environment replaced, and "no usable stored draft" is already the
  // first-visit path — one that seeds defaults rather than guessing.
  if (p.v !== 2) return undefined;
  if (typeof p.model !== "string") return undefined;
  const environment = parseEnvironment(p.environment);
  if (!environment) return undefined;
  if (!isStringArray(p.skills) || !isStringArray(p.mcp) || !isStringArray(p.memorySpaces))
    return undefined;
  return {
    v: 2,
    environment,
    model: p.model,
    skills: p.skills,
    mcp: p.mcp,
    memorySpaces: p.memorySpaces,
    // Added after v1 shipped; older stored drafts simply have no value.
    thinkingEffort: typeof p.thinkingEffort === "string" ? p.thinkingEffort : "",
    // `autoCompact` was a field here and is dropped rather than migrated: a
    // stored draft carrying one is simply ignored, and every session compacts.
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
 * Keep the model and the environment only while they still exist server-side;
 * otherwise fall back to the first model and the default runtime. Returns the
 * same reference when nothing changed so effects can skip redundant writes.
 *
 * `environmentNames` is `undefined` until the list has arrived — a drafted
 * environment is not gone merely because nothing has been fetched yet.
 */
export function reconcileModelEnvironment(
  draft: DraftPayload,
  modelAliases: readonly string[],
  activeVendorNames: readonly string[],
  defaultRuntimeVendor: string,
  environmentNames: readonly string[] | undefined,
): DraftPayload {
  const model = modelAliases.includes(draft.model) ? draft.model : (modelAliases[0] ?? "");
  // Every branch must return the *same object* when nothing actually changed:
  // this runs from an effect that writes its result back, so a fresh object
  // per call — the shape a `{...spread, vendor: default}` naturally produces
  // when the default is itself not in the list — never settles.
  let environment = draft.environment;
  if (environment.kind === "named") {
    if (environmentNames !== undefined && !environmentNames.includes(environment.name)) {
      environment = { kind: "runtime", vendor: defaultRuntimeVendor, repos: {} };
    }
  } else if (
    !activeVendorNames.includes(environment.vendor) &&
    environment.vendor !== defaultRuntimeVendor
  ) {
    environment = { ...environment, vendor: defaultRuntimeVendor };
  }
  if (model === draft.model && environment === draft.environment) return draft;
  return { ...draft, model, environment };
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

/**
 * The draft environment as the wire union.
 *
 * `provisions` decides whether repos travel: a vendor that cannot build a
 * workspace has nowhere to check one out, and sending them anyway earns a
 * rejection at provision time for a selection the picker had already hidden.
 */
export function toEnvironmentSpec(
  environment: EnvironmentDraft,
  provisions: boolean,
): EnvironmentSpec {
  if (environment.kind === "named") {
    return { type: "Named", value: { name: environment.name } };
  }
  const repos: RepoConfig[] = provisions
    ? Object.entries(environment.repos).map(([fullName, ref]) => ({
        url: `https://github.com/${fullName}`,
        gitRef: ref.trim() || undefined,
      }))
    : [];
  return {
    type: "Runtime",
    value: {
      vendor: environment.vendor.trim(),
      repos: repos.length ? repos : undefined,
    },
  };
}

/** `https://github.com/org/repo` → `org/repo`; anything else is kept whole. */
function fullName(url: string): string {
  return url.replace(/^https:\/\/github\.com\//, "").replace(/\.git$/, "");
}

/** The inverse, for a form seeded from something already saved. */
export function fromEnvironmentSpec(spec: EnvironmentSpec): EnvironmentDraft {
  if (spec.type === "Named") return { kind: "named", name: spec.value.name };
  return {
    kind: "runtime",
    vendor: spec.value.vendor,
    repos: Object.fromEntries(
      (spec.value.repos ?? []).map((r) => [fullName(r.url), r.gitRef ?? ""]),
    ),
  };
}
