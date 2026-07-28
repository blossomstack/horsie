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
  /** Canonical thinking effort; "" = the model's configured default. */
  thinkingEffort: string;
  setThinkingEffort: (e: string) => void;
  /** Efforts the selected model offers; empty → no control is shown. */
  thinkingEfforts: string[];
  /** The selected model's default effort, for labelling the fallback option. */
  modelDefaultThinkingEffort: string;
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

  // The menu belongs to the model, so a persisted draft can name an effort the
  // currently-selected model no longer offers. Treat that as "use the default"
  // rather than submitting a value the server would reject with a 422.
  const selectedModel = models.find((m) => m.alias === draft.model);
  const thinkingEfforts = selectedModel?.thinkingEfforts ?? [];
  const effectiveThinkingEffort = thinkingEfforts.includes(draft.thinkingEffort)
    ? draft.thinkingEffort
    : "";

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
        thinkingEffort: effectiveThinkingEffort || undefined,
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
    thinkingEffort: effectiveThinkingEffort,
    setThinkingEffort: (thinkingEffort) => setDraft({ ...draft, thinkingEffort }),
    thinkingEfforts,
    modelDefaultThinkingEffort: selectedModel?.thinkingEffort ?? "",
    provisions,
    githubConnected,
    canSend: blockedReason === null,
    blockedReason,
    buildRequest,
  };
}
