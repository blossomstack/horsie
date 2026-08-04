import type {
  AgentPresetInput,
  AgentView,
  ApiError,
  AgentTokenCreateInput,
  AgentTokenCreated,
  AgentTokenView,
  AuthStatus,
  CreateGroupRequest,
  CreateGroupResponse,
  DeviceApprovalRequest,
  CreateSessionRequest,
  EnvironmentInput,
  EnvironmentView,
  CreateSessionResponse,
  GetSessionResponse,
  GetAgentResponse,
  HistoryPage,
  GitHubAppConfigInput,
  GitHubAppConfigView,
  GitHubBranchList,
  GitHubRepoList,
  GitHubStatus,
  ListGroupsResponse,
  ListSessionsResponse,
  LoginRequest,
  McpAuthorizeUrl,
  McpConnectResult,
  McpServerInput,
  McpServerList,
  McpServerView,
  MemoryCreateInput,
  MemorySpaceCreateInput,
  MemorySpaceUpdateInput,
  MemorySpaceView,
  MemoryUpdateInput,
  MemoryView,
  ModelCard,
  ModelCardInput,
  ModelCardUpdate,
  PluginDefaultInput,
  PluginInstallInput,
  PasswordChangeRequest,
  PluginView,
  RenameGroupRequest,
  RoutineInput,
  RoutineRunResponse,
  RoutineSessionsResponse,
  RoutineView,
  WorkflowInput,
  WorkflowRunGraph,
  WorkflowRunRequest,
  WorkflowRunResponse,
  WorkflowRunsResponse,
  WorkflowView,
  Ack,
  SessionAck,
  SetAnnotationsRequest,
  SettingsUpdate,
  SettingsView,
} from "./types";

// All horsie endpoints live under `/api`. In dev, Vite proxies this prefix to
// the session server (default http://127.0.0.1:3789); in prod the UI is served
// same-origin, so a relative base works everywhere.
const BASE = "/api";

/** A structured error carrying the server's `ApiError` envelope when present. */
export class ApiRequestError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiRequestError";
    this.status = status;
    this.code = code;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(BASE + path, {
      headers: { "Content-Type": "application/json", ...init?.headers },
      ...init,
    });
  } catch (cause) {
    throw new ApiRequestError(
      0,
      "network",
      "Could not reach the horsie server. Is `horsie serve` running?",
    );
  }

  if (!res.ok) {
    let code = `http_${res.status}`;
    let message = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as Partial<ApiError>;
      if (body && typeof body.message === "string") {
        message = body.message;
        if (typeof body.code === "string") code = body.code;
      }
    } catch {
      /* non-JSON error body — keep the status line */
    }
    // A session that expired mid-use should land on the login page, not on a
    // wall of failed queries. The gate listens for this.
    if (res.status === 401) {
      window.dispatchEvent(new Event("horsie:unauthorized"));
    }
    throw new ApiRequestError(res.status, code, message);
  }

  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** The path segment naming a session's primary agent, as opposed to a
 * subagent's uuid. Mirrors the server's own spelling. */
export const MAIN_AGENT = "main";

export const api = {
  health: (): Promise<{ ok: boolean }> => request("/health"),

  auth: {
    status: (): Promise<AuthStatus> => request("/auth/status"),

    login: (password: string): Promise<AuthStatus> =>
      request("/auth/login", {
        method: "POST",
        body: JSON.stringify({ password } satisfies LoginRequest),
      }),

    logout: (): Promise<AuthStatus> =>
      request("/auth/logout", { method: "POST", body: "{}" }),

    changePassword: (body: PasswordChangeRequest): Promise<AuthStatus> =>
      request("/auth/password", { method: "POST", body: JSON.stringify(body) }),

    approveDevice: (userCode: string): Promise<void> =>
      request("/auth/device/approve", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    denyDevice: (userCode: string): Promise<void> =>
      request("/auth/device/deny", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    listTokens: (): Promise<AgentTokenView[]> => request("/auth/tokens"),

    createToken: (label: string): Promise<AgentTokenCreated> =>
      request("/auth/tokens", {
        method: "POST",
        body: JSON.stringify({ label } satisfies AgentTokenCreateInput),
      }),

    deleteToken: (id: string): Promise<void> =>
      request(`/auth/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }),
  },

  sessions: {
    list: (): Promise<ListSessionsResponse> => request("/sessions"),

    get: (id: string): Promise<GetSessionResponse> =>
      request(`/sessions/${encodeURIComponent(id)}`),

    /** A window of one agent's transcript, from its in-memory state.
     * `before` pages backwards (scroll-back), `after` pages forwards (the
     * backfill a reconnecting stream needs); neither requests the tail. */
    history: (
      id: string,
      agentId: string,
      opts: { before?: string; after?: string; limit?: number } = {},
    ): Promise<HistoryPage> => {
      const q = new URLSearchParams();
      if (opts.before) q.set("before", opts.before);
      if (opts.after) q.set("after", opts.after);
      if (opts.limit) q.set("limit", String(opts.limit));
      const qs = q.toString();
      return request(
        `/sessions/${encodeURIComponent(id)}/agents/${encodeURIComponent(agentId)}/history${qs ? `?${qs}` : ""}`,
      );
    },

    create: (body: CreateSessionRequest): Promise<CreateSessionResponse> =>
      request("/sessions", { method: "POST", body: JSON.stringify(body) }),

    remove: (id: string): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}`, { method: "DELETE" }),

    send: (id: string, text: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/messages`, {
        method: "POST",
        body: JSON.stringify({ text }),
      }),

    /** Answer every pending ask at once; a partial set is refused by the server. */
    answerAsks: (
      id: string,
      answers: { toolCallId: string; text: string }[],
    ): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}/answers`, {
        method: "POST",
        body: JSON.stringify({ answers }),
      }),

    stop: (id: string): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}/stop`, {
        method: "POST",
        body: "{}",
      }),

    /** One agent's current values: task list, usage, and — for a subagent —
     * its spawn metadata and terminal result. */
    agent: (id: string, agentId: string): Promise<GetAgentResponse> =>
      request(
        `/sessions/${encodeURIComponent(id)}/agents/${encodeURIComponent(agentId)}`,
      ),

    /** Merge-update a session's annotations (set upserts, remove drops). */
    setAnnotations: (id: string, body: SetAnnotationsRequest): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}/annotations`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),
  },

  sessionGroups: {
    list: (): Promise<ListGroupsResponse> => request("/session-groups"),

    create: (name: string): Promise<CreateGroupResponse> =>
      request("/session-groups", {
        method: "POST",
        body: JSON.stringify({ name } satisfies CreateGroupRequest),
      }),

    rename: (oldName: string, name: string): Promise<Ack> =>
      request(`/session-groups/${encodeURIComponent(oldName)}`, {
        method: "PUT",
        body: JSON.stringify({ name } satisfies RenameGroupRequest),
      }),

    remove: (name: string): Promise<Ack> =>
      request(`/session-groups/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },

  agents: {
    /** All agent presets. */
    list: (): Promise<AgentView[]> => request("/agents"),

    get: (name: string): Promise<AgentView> =>
      request(`/agents/${encodeURIComponent(name)}`),

    create: (body: AgentPresetInput): Promise<AgentView> =>
      request("/agents", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. */
    update: (name: string, body: AgentPresetInput): Promise<AgentView> =>
      request(`/agents/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/agents/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },

  environments: {
    /** All environments. */
    list: (): Promise<EnvironmentView[]> => request("/environments"),

    get: (name: string): Promise<EnvironmentView> =>
      request(`/environments/${encodeURIComponent(name)}`),

    create: (body: EnvironmentInput): Promise<EnvironmentView> =>
      request("/environments", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. */
    update: (name: string, body: EnvironmentInput): Promise<EnvironmentView> =>
      request(`/environments/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/environments/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },

  routines: {
    /** All routines. */
    list: (): Promise<RoutineView[]> => request("/routines"),

    get: (name: string): Promise<RoutineView> =>
      request(`/routines/${encodeURIComponent(name)}`),

    create: (body: RoutineInput): Promise<RoutineView> =>
      request("/routines", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. */
    update: (name: string, body: RoutineInput): Promise<RoutineView> =>
      request(`/routines/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    /** Deletes the routine *and* every session it created. */
    remove: (name: string): Promise<void> =>
      request(`/routines/${encodeURIComponent(name)}`, { method: "DELETE" }),

    /** Trigger now, whatever the schedule says. Returns as soon as the
     * session exists; the run itself continues in the background. */
    run: (name: string): Promise<RoutineRunResponse> =>
      request(`/routines/${encodeURIComponent(name)}/run`, {
        method: "POST",
        body: "{}",
      }),

    /** The routine's runs, newest first. They are deliberately absent from
     * the session list. */
    sessions: (name: string): Promise<RoutineSessionsResponse> =>
      request(`/routines/${encodeURIComponent(name)}/sessions`),
  },

  workflows: {
    /** All workflow definitions. */
    list: (): Promise<WorkflowView[]> => request("/workflows"),

    get: (name: string): Promise<WorkflowView> =>
      request(`/workflows/${encodeURIComponent(name)}`),

    create: (body: WorkflowInput): Promise<WorkflowView> =>
      request("/workflows", { method: "POST", body: JSON.stringify(body) }),

    /** Full replace; the path is the id of record. A run already under way
     * keeps the graph it started with. */
    update: (name: string, body: WorkflowInput): Promise<WorkflowView> =>
      request(`/workflows/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/workflows/${encodeURIComponent(name)}`, { method: "DELETE" }),

    /** Start a run. Returns as soon as the session exists; the first step is
     * already on its way. */
    run: (name: string, body: WorkflowRunRequest): Promise<WorkflowRunResponse> =>
      request(`/workflows/${encodeURIComponent(name)}/runs`, {
        method: "POST",
        body: JSON.stringify(body),
      }),

    /** This workflow's runs, newest first. Unlike a routine's, these are also
     * in the ordinary session list. */
    runs: (name: string): Promise<WorkflowRunsResponse> =>
      request(`/workflows/${encodeURIComponent(name)}/runs`),

    /** A run, projected onto its definition's graph. Keyed by session id,
     * because that is what a run is. */
    graph: (sessionId: string): Promise<WorkflowRunGraph> =>
      request(`/sessions/${encodeURIComponent(sessionId)}/workflow`),

    /** Re-run one execution. Appends an attempt; never truncates. */
    retry: (sessionId: string, stepIndex: number): Promise<void> =>
      request(`/sessions/${encodeURIComponent(sessionId)}/workflow/retry`, {
        method: "POST",
        body: JSON.stringify({ stepIndex }),
      }),
  },

  config: {
    /** The current redacted settings (providers, models, vendors, deployment info). */
    get: (): Promise<SettingsView> => request("/config"),

    /** Persist + live-apply a settings update; returns the new view. */
    update: (body: SettingsUpdate): Promise<SettingsView> =>
      request("/config", { method: "PUT", body: JSON.stringify(body) }),

    /** On-demand reachability + token check for a vendor (velos only); never mutates settings. */
  },

  modelCards: {
    /** Public: cards whose modelId starts with `prefix` (all when ""). */
    search: (prefix = ""): Promise<ModelCard[]> =>
      request(
        `/model-cards${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ""}`,
      ),
  },

  admin: {
    modelCards: {
      list: (): Promise<ModelCard[]> => request("/admin/model-cards"),

      create: (body: ModelCardInput): Promise<ModelCard> =>
        request("/admin/model-cards", {
          method: "POST",
          body: JSON.stringify(body),
        }),

      /** Update name/limits; `modelId` is immutable. */
      update: (modelId: string, body: ModelCardUpdate): Promise<ModelCard> =>
        request(`/admin/model-cards/${encodeURIComponent(modelId)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        }),

      remove: (modelId: string): Promise<void> =>
        request(`/admin/model-cards/${encodeURIComponent(modelId)}`, {
          method: "DELETE",
        }),
    },
  },

  github: {
    status: (): Promise<GitHubStatus> => request("/github/status"),

    /** Browser navigation target (not fetch) — starts the OAuth flow. */
    authUrl: (): string => `${BASE}/github/auth`,

    appConfig: (): Promise<GitHubAppConfigView> =>
      request("/github/app-config"),

    saveAppConfig: (body: GitHubAppConfigInput): Promise<GitHubAppConfigView> =>
      request("/github/app-config", {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    disconnect: (): Promise<void> =>
      request("/github/disconnect", { method: "DELETE" }),

    repos: (refresh = false): Promise<GitHubRepoList> =>
      request(`/github/repos${refresh ? "?refresh=1" : ""}`),

    branches: (repo: string): Promise<GitHubBranchList> =>
      request(`/github/repos/branches?repo=${encodeURIComponent(repo)}`),
  },

  plugins: {
    /** All installed skill bundles (metadata only). */
    list: (): Promise<PluginView[]> => request("/plugins"),

    /** Install a bundle from a git repo; may take a few seconds. */
    install: (body: PluginInstallInput): Promise<PluginView> =>
      request("/plugins", { method: "POST", body: JSON.stringify(body) }),

    /** Re-clone a bundle at its ref to pick up upstream changes. */
    update: (name: string): Promise<PluginView> =>
      request(`/plugins/${encodeURIComponent(name)}/update`, {
        method: "POST",
      }),

    /** Toggle whether a bundle is pre-selected for new sessions. */
    setDefault: (name: string, body: PluginDefaultInput): Promise<PluginView> =>
      request(`/plugins/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/plugins/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },

  memory: {
    /** All memory spaces, each with its memory count. */
    listSpaces: (): Promise<MemorySpaceView[]> => request("/memory-spaces"),

    createSpace: (body: MemorySpaceCreateInput): Promise<MemorySpaceView> =>
      request("/memory-spaces", { method: "POST", body: JSON.stringify(body) }),

    /** Rename and/or re-describe; renaming carries the space's memories. */
    updateSpace: (
      name: string,
      body: MemorySpaceUpdateInput,
    ): Promise<MemorySpaceView> =>
      request(`/memory-spaces/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    /** Delete a space and every memory in it. */
    deleteSpace: (name: string): Promise<void> =>
      request(`/memory-spaces/${encodeURIComponent(name)}`, {
        method: "DELETE",
      }),

    /** Memories, optionally limited to one space. */
    list: (space?: string): Promise<MemoryView[]> =>
      request(
        space ? `/memories?space=${encodeURIComponent(space)}` : "/memories",
      ),

    create: (body: MemoryCreateInput): Promise<MemoryView> =>
      request("/memories", { method: "POST", body: JSON.stringify(body) }),

    update: (id: number, body: MemoryUpdateInput): Promise<MemoryView> =>
      request(`/memories/${id}`, { method: "PUT", body: JSON.stringify(body) }),

    remove: (id: number): Promise<void> =>
      request(`/memories/${id}`, { method: "DELETE" }),
  },

  mcp: {
    /** The configured remote MCP servers, redacted (tokens as `hasToken`). */
    list: (): Promise<McpServerList> => request("/mcp/servers"),

    /** Upsert a server by name (the path is the id of record). */
    upsert: (name: string, body: McpServerInput): Promise<McpServerView> =>
      request(`/mcp/servers/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/mcp/servers/${encodeURIComponent(name)}`, { method: "DELETE" }),

    /** Connect (`initialize` + `tools/list`); persists + returns the outcome. */
    test: (name: string): Promise<McpConnectResult> =>
      request(`/mcp/servers/${encodeURIComponent(name)}/test`, {
        method: "POST",
        body: "{}",
      }),

    /** Begin OAuth for an `oauth` server; returns the authorize URL to navigate to. */
    connect: (name: string): Promise<McpAuthorizeUrl> =>
      request(`/mcp/servers/${encodeURIComponent(name)}/connect`, {
        method: "POST",
        body: "{}",
      }),
  },

  /** SSE URL for a session's own stream: status, inbox, progression, errors,
   * and agent-roster changes. Session-scoped current values only — no
   * transcript, no cursor. */
  sessionEventsUrl: (id: string): string =>
    `${BASE}/sessions/${encodeURIComponent(id)}/events`,

  /** SSE URL for one agent's stream: transcript appends (id-stamped with the
   * message id, so the browser resumes from them automatically), plus that
   * agent's task list, usage, and live run frames. */
  agentEventsUrl: (id: string, agentId: string): string =>
    `${BASE}/sessions/${encodeURIComponent(id)}/agents/${encodeURIComponent(agentId)}/events`,

  /** SSE URL for the global session-status feed. */
  globalEventsUrl: (): string => `${BASE}/events`,
};
