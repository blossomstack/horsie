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
  /** Canonical thinking effort; "" = use the model's configured default. */
  thinkingEffort: string;
}

export function emptyDraft(): DraftPayload {
  return {
    v: 1,
    vendor: "",
    model: "",
    repos: {},
    skills: [],
    mcp: [],
    memorySpaces: [],
    thinkingEffort: "",
  };
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
    // Added after v1 shipped; older stored drafts simply have no value.
    thinkingEffort: typeof p.thinkingEffort === "string" ? p.thinkingEffort : "",
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
