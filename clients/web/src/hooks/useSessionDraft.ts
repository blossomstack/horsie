import { useEffect, useMemo, useState } from "react";
import type {
  AgentInvokeRequest,
  CreateSessionRequest,
  EnvironmentSpec,
  EnvironmentView,
  WorkflowRunRequest,
} from "../api/types";
import {
  DRAFT_STORAGE_KEY,
  emptyDraft,
  filterMcpServers,
  filterMemorySpaces,
  filterSkills,
  filterTools,
  loadDraftPayload,
  parseDraftPayload,
  reconcileModelEnvironment,
  toEnvironmentSpec,
  type DraftPayload,
  type EnvironmentDraft,
} from "./draftPersistence";
import { useAgents } from "./useAgents";
import { useEnvironments } from "./useEnvironments";
import { useGithubStatus } from "./useGithub";
import { useMemorySpaces } from "./useMemory";
import { useMcpServers } from "./useMcp";
import { usePersistentState } from "./usePersistentState";
import { usePlugins } from "./usePlugins";
import { useSettings } from "./useSettings";
import { allTools, useTools } from "./useTools";
import { useWorkflows } from "./useWorkflows";

export type { EnvironmentDraft };

/** The picker state every session-config surface shares: the new-session
 * draft and the agent-preset form. `SessionDraft` adds what only sending a
 * first message needs. */
/**
 * The channels a session and an agent preset both configure.
 *
 * Deliberately no environment. A preset does not name one — where the work
 * runs and what it runs against belong to the invocation, not to the saved
 * configuration — so the environment channel lives in `EnvironmentChannel`,
 * which only a session draft has.
 */
export interface ConfigDraft {
  model: string;
  setModel: (m: string) => void;
  skills: Set<string>;
  setSkills: (s: Set<string>) => void;
  mcp: Set<string>;
  setMcp: (s: Set<string>) => void;
  /** Memory spaces the session may read and write. */
  memorySpaces: Set<string>;
  setMemorySpaces: (s: Set<string>) => void;
  /**
   * The built-in tools the agent may call. `null` means the server's default
   * set, and is what an untouched draft carries.
   *
   * `null` is not "all": it is deliberately a *deferred* answer, so a preset
   * saved today follows a later horsie's idea of sensible instead of freezing
   * this one's list — and so no unset field can hand out the control plane. An
   * empty set is a real answer meaning no built-in tools at all.
   */
  tools: Set<string> | null;
  setTools: (t: Set<string> | null) => void;
  /** Canonical thinking effort; "" = the model's configured default. */
  thinkingEffort: string;
  setThinkingEffort: (e: string) => void;
  /** Efforts the selected model offers; empty → no control is shown. */
  thinkingEfforts: string[];
  /** The selected model's default effort, for labelling the fallback option. */
  modelDefaultThinkingEffort: string;
}

/**
 * The environment channel, which a session has and an agent preset does not.
 * Its presence is what tells `useConfigPickers` to offer an Environment picker.
 *
 * `provisions` and `githubConnected` ride here rather than on `ConfigDraft`
 * because both exist to answer one question — can this selection hold repos —
 * and that question only has meaning once something has been selected.
 */
export interface EnvironmentChannel {
  environment: EnvironmentDraft;
  setEnvironment: (e: EnvironmentDraft) => void;
  /** Predefined environments, for the picker's first section. */
  environments: EnvironmentView[];
  /** The resolved vendor can build a workspace, so repos mean something. */
  provisions: boolean;
  githubConnected: boolean;
}

/** A predefined agent replaces the direct model and toolbox configuration. */
export interface AgentChannel {
  /** Selected predefined agent; empty means configure a model directly. */
  agent: string;
  setAgent: (agent: string) => void;
  /** Agent presets available for direct invocation. */
  agents: string[];
}

/** A workflow run replaces the direct model and toolbox configuration. */
export interface WorkflowChannel {
  workflow: string;
  setWorkflow: (w: string) => void;
  /** Every definition on the server, for the picker. */
  workflows: string[];
}

export interface SessionDraft
  extends ConfigDraft,
    EnvironmentChannel,
    AgentChannel,
    WorkflowChannel {
  canSend: boolean;
  blockedReason: string | null;
  /** A session is created with its first message; there is no create-only call. */
  buildRequest: (message: string) => CreateSessionRequest;
  /** Request for an invocation configured by a predefined agent. */
  buildAgentRequest: (message: string) => AgentInvokeRequest;
  /** The same channels as a workflow run. Only meaningful when `workflow` is set. */
  buildRunRequest: (input: string) => WorkflowRunRequest;
}

/**
 * @param initialWorkflow preselects the workflow channel — the workflow page's
 * `Run` link arrives here with one in the query string.
 */
export function useSessionDraft(initialWorkflow = ""): SessionDraft {
  const { data: settings, isError: settingsFailed } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const { data: bundles } = usePlugins();
  const { data: mcpServers } = useMcpServers();
  const { data: memorySpaces } = useMemorySpaces();
  const { data: workflows } = useWorkflows();
  const { data: agents } = useAgents();
  const { data: environments } = useEnvironments();
  const { data: toolCatalog } = useTools();
  const models = settings?.models ?? [];
  const activeVendors = useMemo(() => settings?.vendors ?? [], [settings]);

  // Load-once snapshot: `undefined` means this browser has no usable stored
  // draft (first visit, corrupt payload, unknown version) — the signal that
  // decides whether default-enabled bundles get seeded below.
  const [storedAtMount] = useState(() => loadDraftPayload());
  const [draft, setDraft] = usePersistentState<DraftPayload>(
    DRAFT_STORAGE_KEY,
    storedAtMount ?? emptyDraft(),
    { deserialize: parseDraftPayload },
  );

  // Deliberately outside the persisted payload: every other channel tunes a
  // session, but this one replaces what the page starts. Coming back days
  // later and silently being in workflow mode is not a setting anyone chose.
  const [workflow, setWorkflow] = useState(initialWorkflow);
  // Like workflows, a preset replaces what this page starts, so it is never
  // restored as a surprise on a later visit.
  const [agent, setAgent] = useState("");
  const workflowNames = useMemo(
    () => (workflows ?? []).map((w) => w.name),
    [workflows],
  );
  // A name from the query string, or one deleted since it was picked, is not a
  // workflow. Until the list arrives nothing is known to be missing, so the
  // preselection holds rather than flickering through plain-session mode.
  const selectedWorkflow =
    workflows === undefined || workflowNames.includes(workflow) ? workflow : "";

  const agentNames = useMemo(() => (agents ?? []).map((a) => a.name), [agents]);
  const selectedAgent = agents === undefined || agentNames.includes(agent) ? agent : "";

  const environmentNames = useMemo(
    () => (environments === undefined ? undefined : environments.map((e) => e.name)),
    [environments],
  );

  // Keep model/environment on still-existing choices as server config changes.
  useEffect(() => {
    if (!settings) return;
    const next = reconcileModelEnvironment(
      draft,
      models.map((m) => m.alias),
      activeVendors.map((v) => v.name),
      settings.defaultRuntimeVendor,
      environmentNames,
    );
    if (next !== draft) setDraft(next);
  }, [settings, models, activeVendors, environmentNames, draft]);

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

  // The same pass for tools, but on its own: the catalogue is a separate read,
  // and folding it into the one above would let a failed `/api/tools` stop
  // skills, MCP servers and memory spaces from ever being reconciled.
  const [toolsFiltered, setToolsFiltered] = useState(false);
  useEffect(() => {
    if (toolsFiltered || !toolCatalog) return;
    const next = filterTools(draft, new Set(allTools(toolCatalog).map((t) => t.name)));
    if (next !== draft) setDraft(next);
    setToolsFiltered(true);
  }, [toolsFiltered, toolCatalog, draft]);

  const environment = draft.environment;
  // Which vendor this selection resolves to — its own, or the predefined
  // environment's. One lookup, so every downstream answer agrees.
  const resolvedVendor =
    environment.kind === "named"
      ? (environments ?? []).find((e) => e.name === environment.name)?.vendor
      : environment.vendor;
  const provisions = !!activeVendors.find((v) => v.name === resolvedVendor)
    ?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  const chosen =
    environment.kind === "named"
      ? !!environment.name
      : !!environment.vendor.trim();
  // Only an ad-hoc selection with repos needs GitHub: a predefined
  // environment's repos were resolved when it was saved.
  const needsGithub =
    environment.kind === "runtime" &&
    provisions &&
    Object.keys(environment.repos).length > 0;

  const blockedReason = useMemo(() => {
    // Models and runtimes both come from `/api/config`. With that read dead,
    // every picker below is empty for a reason that has nothing to do with the
    // draft — telling someone to select a model from a menu that says there
    // are none is the one thing this must not do.
    if (settingsFailed)
      return "Couldn’t load this server’s models and runtimes. Reload once the server is reachable.";
    // A run takes its model from each step's preset, so the model channel is
    // neither shown nor required while a workflow is selected.
    if (!selectedWorkflow && !selectedAgent && !draft.model.trim())
      return "Select a model or agent to start.";
    if (!chosen) return "Select an environment to start.";
    if (needsGithub && !githubConnected) return "Connect GitHub to use these repos.";
    return null;
  }, [
    settingsFailed,
    draft.model,
    chosen,
    needsGithub,
    githubConnected,
    selectedWorkflow,
    selectedAgent,
  ]);

  // The menu belongs to the model, so a persisted draft can name an effort the
  // currently-selected model no longer offers. Treat that as "use the default"
  // rather than submitting a value the server would reject with a 422.
  const selectedModel = models.find((m) => m.alias === draft.model);
  const thinkingEfforts = selectedModel?.thinkingEfforts ?? [];
  const effectiveThinkingEffort = thinkingEfforts.includes(draft.thinkingEffort)
    ? draft.thinkingEffort
    : "";

  const environmentSpec = (): EnvironmentSpec =>
    toEnvironmentSpec(environment, provisions);

  const buildRequest = (message: string): CreateSessionRequest => ({
    message,
    agent: {
      model: draft.model.trim(),
      // Not gated on the environment — see `plugins` below.
      usePlugins: true,
      // Nor is MCP: a toolbox is composed server-side and never reaches the
      // runtime at all.
      mcpServers: draft.mcp.length ? draft.mcp : undefined,
      // Memories are served by the server itself, so they work on every
      // vendor, including ones that can't provision.
      memorySpaces: draft.memorySpaces.length ? draft.memorySpaces : undefined,
      // `null` — nothing was chosen — sends nothing, which the server reads as
      // its default set. An empty array is sent as itself: "no built-in tools"
      // is a choice, and `|| undefined` would quietly overturn it.
      allowedTools: draft.tools ?? undefined,
      thinkingEffort: effectiveThinkingEffort || undefined,
      // `autoCompact` is left absent, which the server reads as on. The UI
      // stopped offering the choice; sending today's default explicitly would
      // freeze it into every session ever created.
    },
    environment: environmentSpec(),
    // Bundles are not a workspace: the runtime fetches them over its own
    // outbound connection into a directory of its own, which it can do
    // whether or not it provisioned anything. Gating this on provisioning
    // meant the picker took a selection on `horsie connect` — the most
    // common self-hosted vendor — and silently dropped it. The same
    // one-bit-three-jobs mistake #178 fixed on agent presets.
    plugins: draft.skills.length ? draft.skills : undefined,
  });

  // A run carries no agent configuration: the graph names a preset per step,
  // and the snapshot taken at creation resolves each one server-side.
  const buildAgentRequest = (message: string): AgentInvokeRequest => ({
    message,
    environment: environmentSpec(),
  });

  const buildRunRequest = (input: string): WorkflowRunRequest => ({
    input,
    environment: environmentSpec(),
  });

  return {
    workflow: selectedWorkflow,
    setWorkflow: (next) => {
      setWorkflow(next);
      if (next) setAgent("");
    },
    workflows: workflowNames,
    agent: selectedAgent,
    setAgent: (next) => {
      setAgent(next);
      if (next) setWorkflow("");
    },
    agents: agentNames,
    environment,
    setEnvironment: (next) => setDraft({ ...draft, environment: next }),
    environments: environments ?? [],
    model: draft.model,
    setModel: (model) => setDraft({ ...draft, model }),
    skills: new Set(draft.skills),
    setSkills: (skills) => setDraft({ ...draft, skills: [...skills] }),
    mcp: new Set(draft.mcp),
    setMcp: (mcp) => setDraft({ ...draft, mcp: [...mcp] }),
    memorySpaces: new Set(draft.memorySpaces),
    setMemorySpaces: (memorySpaces) =>
      setDraft({ ...draft, memorySpaces: [...memorySpaces] }),
    tools: draft.tools === null ? null : new Set(draft.tools),
    setTools: (tools) => setDraft({ ...draft, tools: tools === null ? null : [...tools] }),
    thinkingEffort: effectiveThinkingEffort,
    setThinkingEffort: (thinkingEffort) => setDraft({ ...draft, thinkingEffort }),
    thinkingEfforts,
    modelDefaultThinkingEffort: selectedModel?.thinkingEffort ?? "",
    provisions,
    githubConnected,
    canSend: blockedReason === null,
    blockedReason,
    buildRequest,
    buildAgentRequest,
    buildRunRequest,
  };
}
