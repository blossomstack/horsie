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
  MessagesPage,
  GitHubAppConfigInput,
  GitHubAppConfigView,
  GitHubBranchList,
  GitHubRepoList,
  GitHubStatus,
  ListGroupsResponse,
  ListSessionsResponse,
  LoginRequest,
  InstallOutcome,
  MarketplaceView,
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
  RuntimeVendorConfigInput,
  RuntimeVendorConfigView,
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
  SettingsView,
  ModelInput,
  ModelView,
  ProviderInput,
  ProviderView,
} from "./types";

// All horsie endpoints live under `/api`. In dev, Vite proxies this prefix to
// the session server (default http://127.0.0.1:3789); in prod the UI is served
// same-origin, so a relative base works everywhere.
const BASE = "/api";

// The ChatGPT sign-in bodies are declared here rather than in a `.fl` schema:
// they are three small admin-only shapes with no other client, and adding them
// to fluorite would regenerate both type trees for nothing.

/** Whether a `chatgpt` provider has a stored sign-in. */
export type ChatGptStatus = { signedIn: boolean; accountId?: string };

/** A device-code login waiting to be approved in a browser. */
export type ChatGptStartedLogin = {
  userCode: string;
  verificationUrl: string;
  /** How often OpenAI wants to be polled. Faster earns a rate limit. */
  intervalSecs: number;
};

export type ChatGptPoll = {
  status: "pending" | "complete";
  accountId?: string;
};

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
    // `statusText` is empty over HTTP/2, so the old fallback of
    // `${status} ${statusText}` rendered as a bare `422 ` with a trailing
    // space. The status alone at least says something.
    let message = res.statusText ? `${res.status} ${res.statusText}` : `${res.status}`;
    // Read once, as text, then try to parse it. The old code called
    // `res.json()`, which *throws* on a non-JSON body — and axum's own body
    // rejections are `text/plain`. So the server's real message
    // ("provision[0]: missing field `name` at line 1 column 63") was discarded
    // and the user saw the bare status instead.
    const raw = await res.text().catch(() => "");
    let parsed: Partial<ApiError> | undefined;
    try {
      parsed = JSON.parse(raw) as Partial<ApiError>;
    } catch {
      /* not JSON — the text itself is the message */
    }
    if (parsed && typeof parsed.message === "string") {
      message = parsed.message;
      if (typeof parsed.code === "string") code = parsed.code;
    } else if (raw.trim()) {
      message = raw.trim();
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
      request("/device/approve", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    denyDevice: (userCode: string): Promise<void> =>
      request("/device/deny", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    listTokens: (): Promise<AgentTokenView[]> => request("/device/tokens"),

    createToken: (label: string): Promise<AgentTokenCreated> =>
      request("/device/tokens", {
        method: "POST",
        body: JSON.stringify({ label } satisfies AgentTokenCreateInput),
      }),

    deleteToken: (id: string): Promise<void> =>
      request(`/device/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }),
  },

  sessions: {
    list: (): Promise<ListSessionsResponse> => request("/sessions"),

    get: (id: string): Promise<GetSessionResponse> =>
      request(`/sessions/${encodeURIComponent(id)}`),

    /** A window of one agent's log, ending just before `before`.
     *
     * Scroll-back only. Forward reading is the stream — the same endpoint
     * without `before` — so there is no second way to page forwards and no
     * backfill loop to keep in step with a subscription. */
    messages: (
      id: string,
      agentId: string,
      opts: { before?: number; max?: number } = {},
    ): Promise<MessagesPage> => {
      const q = new URLSearchParams({ aid: agentId });
      if (opts.before !== undefined) q.set("before", String(opts.before));
      if (opts.max) q.set("max", String(opts.max));
      return request(
        `/sessions/${encodeURIComponent(id)}/messages?${q.toString()}`,
      );
    },

    create: (body: CreateSessionRequest): Promise<CreateSessionResponse> =>
      request("/sessions", { method: "POST", body: JSON.stringify(body) }),

    remove: (id: string): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}`, { method: "DELETE" }),

    /** Rename a session. Single line, non-empty, at most 60 characters. */
    rename: (id: string, name: string): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}/name`, {
        method: "PUT",
        body: JSON.stringify({ name }),
      }),

    send: (id: string, text: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/messages`, {
        method: "POST",
        body: JSON.stringify({ text }),
      }),

    /** Answer every pending ask at once; a partial set is refused by the server.
     *
     * `agentId` names who asked. It is not optional: the questions belong to one
     * agent, and a workflow run has no main agent to fall back to — sending this
     * unaddressed there resolved nothing and silently did nothing. */
    answerAsks: (
      id: string,
      agentId: string,
      answers: { toolCallId: string; text: string }[],
    ): Promise<Ack> =>
      request(
        `/sessions/${encodeURIComponent(id)}/answers?aid=${encodeURIComponent(agentId)}`,
        {
          method: "POST",
          body: JSON.stringify({ answers }),
        },
      ),

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

  /**
   * Runtime vendors the server builds itself. The ones that dial in announce
   * themselves and appear in the settings view instead — there is nothing to
   * create or delete about a process someone else is running.
   */
  runtimeVendors: {
    list: (): Promise<RuntimeVendorConfigView[]> => request("/runtime-vendors"),

    /** Create or fully replace. Omit `credential` to keep the stored token. */
    save: (
      name: string,
      body: RuntimeVendorConfigInput,
    ): Promise<RuntimeVendorConfigView> =>
      request(`/runtime-vendors/${encodeURIComponent(name)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (name: string): Promise<void> =>
      request(`/runtime-vendors/${encodeURIComponent(name)}`, {
        method: "DELETE",
      }),
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

    /** The runtime vendor new sessions default to; returns the new view. */
    setDefaultRuntimeVendor: (vendor: string): Promise<SettingsView> =>
      request("/config/default-runtime-vendor", {
        method: "PUT",
        body: JSON.stringify({ vendor }),
      }),

    /** Forget the default-runtime-vendor preference, falling back to the built-in. */
    clearDefaultRuntimeVendor: (): Promise<SettingsView> =>
      request("/config/default-runtime-vendor", { method: "DELETE" }),

    /** Model aliases, one resource at a time. */
    models: {
      list: (): Promise<ModelView[]> => request("/config/models"),

      /** Create or replace one alias. The path is the identity. */
      put: (alias: string, body: ModelInput): Promise<ModelView> =>
        request(`/config/models/${encodeURIComponent(alias)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        }),

      remove: (alias: string): Promise<void> =>
        request(`/config/models/${encodeURIComponent(alias)}`, {
          method: "DELETE",
        }),
    },

    /** LLM providers. Named `model-providers` server-side because a bare
     * "provider" collides with the runtime vendor vocabulary. */
    modelProviders: {
      list: (): Promise<ProviderView[]> => request("/config/model-providers"),

      /** Omitting `apiKey` keeps the stored key; `""` clears it. */
      put: (name: string, body: ProviderInput): Promise<ProviderView> =>
        request(`/config/model-providers/${encodeURIComponent(name)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        }),

      /** 409 while any model still routes to it. */
      remove: (name: string): Promise<void> =>
        request(`/config/model-providers/${encodeURIComponent(name)}`, {
          method: "DELETE",
        }),
    },
  },

  modelCards: {
    /** Public: cards whose modelId starts with `prefix` (all when ""). */
    search: (prefix = ""): Promise<ModelCard[]> =>
      request(
        `/model-cards${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ""}`,
      ),
  },

  admin: {
    /** ChatGPT-plan sign-in for `kind: "chatgpt"` providers. Device code, so
     * the browser half happens on OpenAI's site and this server never sees a
     * ChatGPT password. */
    chatgpt: {
      status: (provider: string): Promise<ChatGptStatus> =>
        request(`/config/model-providers/${encodeURIComponent(provider)}/chatgpt`),

      /** Ask OpenAI for a user code. The operator types it at `verificationUrl`. */
      start: (provider: string): Promise<ChatGptStartedLogin> =>
        request(`/config/model-providers/${encodeURIComponent(provider)}/chatgpt/login`, {
          method: "POST",
          body: "{}",
        }),

      /** One poll. `pending` until the operator approves it in their browser. */
      poll: (provider: string): Promise<ChatGptPoll> =>
        request(`/config/model-providers/${encodeURIComponent(provider)}/chatgpt/poll`, {
          method: "POST",
          body: "{}",
        }),

      signOut: (provider: string): Promise<void> =>
        request(`/config/model-providers/${encodeURIComponent(provider)}/chatgpt/login`, {
          method: "DELETE",
        }),
    },

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

    /**
     * Install a bundle, or register the catalogue the URL turned out to be.
     * One box: the server classifies what it cloned. May take a few seconds.
     */
    install: (body: PluginInstallInput): Promise<InstallOutcome> =>
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

  marketplaces: {
    /** Registered sources, each carrying its cached catalogue. */
    list: (): Promise<MarketplaceView[]> => request("/marketplaces"),

    /** Re-clone and re-parse a source's index; may take a few seconds. */
    refresh: (name: string): Promise<MarketplaceView> =>
      request(`/marketplaces/${encodeURIComponent(name)}/refresh`, {
        method: "POST",
      }),

    /** Drop a source. Bundles installed from it stay installed. */
    remove: (name: string): Promise<void> =>
      request(`/marketplaces/${encodeURIComponent(name)}`, { method: "DELETE" }),
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

  /** SSE URL for one agent's log: every entry from the start, then live.
   *
   * One stream where there were two. Session-scoped facts — status, the queue,
   * provisioning progress, turn boundaries — are entries in this log, so
   * nothing has to be ordered against a second source. Every frame carries an
   * SSE id, so the browser's own `Last-Event-ID` is the resume cursor. */
  messagesUrl: (id: string, agentId: string): string =>
    `${BASE}/sessions/${encodeURIComponent(id)}/messages?aid=${encodeURIComponent(agentId)}`,

  /** SSE URL for the global session-status feed. */
  globalEventsUrl: (): string => `${BASE}/events`,
};
