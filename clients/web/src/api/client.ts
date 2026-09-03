import type {
  AgentInvokeRequest,
  AgentInvokeResponse,
  AgentPresetInput,
  AgentView,
  ApiError,
  ArtifactRef,
  AgentTokenCreateInput,
  AgentTokenCreated,
  AgentTokenView,
  AuthStatus,
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
  InboxListResponse,
  InboxMessageIds,
  InboxReplyRequest,
  ListSessionsResponse,
  LoginRequest,
  InstallOutcome,
  MarketplaceView,
  McpAuthorizeUrl,
  McpConnectResult,
  McpServerDetail,
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
  RuntimeVendorTestResult,
  ModelCardInput,
  ModelCardUpdate,
  PluginDefaultInput,
  PluginInstallInput,
  PasswordChangeRequest,
  CatalogEntryView,
  AuthoredFileView,
  AuthoredPluginView,
  AuthoredPluginWriteInput,
  AuthoredRevisionView,
  AuthoredSkillView,
  PluginView,
  ProjectInput,
  ProjectView,
  ToolCatalog,
  RoutineInput,
  RoutineRunResponse,
  RoutineView,
  WorkflowInput,
  WorkflowRunGraph,
  WorkflowRunRequest,
  WorkflowRunResponse,
  WorkflowView,
  Ack,
  SessionAck,
  SendMessageRequest,
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

/**
 * The project every scoped request goes to.
 *
 * Module state rather than an argument on eighty functions, and rather than a
 * React context this non-React module cannot read: a browser tab is in exactly
 * one project at a time, and the `/p/:project` route is what says which.
 * `ProjectScope` sets it while rendering — before any query below runs — and
 * clears React Query's cache when it changes, so a switch refetches rather than
 * painting the previous project's data under the new project's name.
 */
let currentProject: string | null = null;

export function setCurrentProject(id: string): void {
  currentProject = id;
}

export function getCurrentProject(): string | null {
  return currentProject;
}

/**
 * A scoped path, prefixed with the project.
 *
 * Throws rather than falling back to some default when no project is set: a
 * default would make a routing bug look like an empty account, which is the
 * failure mode this whole design is built to avoid.
 */
function scoped(path: string): string {
  if (!currentProject) {
    throw new ApiRequestError(
      0,
      "no_project",
      "No project selected — this is a routing bug, not something to retry.",
    );
  }
  return `${BASE}/p/${currentProject}${path}`;
}

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

/** A scoped request: `path` is relative to the current project. */
async function request<T>(path: string, init?: RequestInit): Promise<T> {
  return send<T>(scoped(path), init);
}

/**
 * A request that belongs to no project: the credential routes, and `/projects`
 * itself — which is how a client learns what may go in the scoped prefix.
 */
async function unscoped<T>(path: string, init?: RequestInit): Promise<T> {
  return send<T>(BASE + path, init);
}

async function send<T>(url: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(url, {
      ...init,
      // Merged *after* `init` is spread, not before. Spread first, the
      // caller's whole `headers` object replaced the default one — which
      // happened to give an artifact upload the behaviour it wanted and
      // would have silently dropped any header added beside it.
      headers: { "Content-Type": "application/json", ...init?.headers },
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

/** Which slice of the inbox to read, spelled as the server's `state` parameter
 * spells it. An unknown value is refused there rather than widened to
 * everything, so this is a closed set on purpose. */
export type InboxScope = "all" | "open" | "unread";

export const api = {
  health: (): Promise<{ ok: boolean }> => unscoped("/health"),

  auth: {
    status: (): Promise<AuthStatus> => unscoped("/auth/status"),

    login: (password: string): Promise<AuthStatus> =>
      unscoped("/auth/login", {
        method: "POST",
        body: JSON.stringify({ password } satisfies LoginRequest),
      }),

    logout: (): Promise<AuthStatus> =>
      unscoped("/auth/logout", { method: "POST", body: "{}" }),

    changePassword: (body: PasswordChangeRequest): Promise<AuthStatus> =>
      unscoped("/auth/password", { method: "POST", body: JSON.stringify(body) }),

    approveDevice: (userCode: string): Promise<void> =>
      unscoped("/device/approve", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    denyDevice: (userCode: string): Promise<void> =>
      unscoped("/device/deny", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    listTokens: (): Promise<AgentTokenView[]> => unscoped("/device/tokens"),

    createToken: (label: string): Promise<AgentTokenCreated> =>
      unscoped("/device/tokens", {
        method: "POST",
        body: JSON.stringify({ label } satisfies AgentTokenCreateInput),
      }),

    deleteToken: (id: string): Promise<void> =>
      unscoped(`/device/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }),
  },

  /**
   * An account's projects — the one resource that is not inside one.
   *
   * `list` is also what creates the default project on an account's first
   * visit, which is why the router calls it before anything else.
   */
  projects: {
    list: (): Promise<ProjectView[]> => unscoped("/projects"),

    create: (name: string): Promise<ProjectView> =>
      unscoped("/projects", {
        method: "POST",
        body: JSON.stringify({ name } satisfies ProjectInput),
      }),

    rename: (id: string, name: string): Promise<ProjectView> =>
      unscoped(`/projects/${encodeURIComponent(id)}`, {
        method: "PUT",
        body: JSON.stringify({ name } satisfies ProjectInput),
      }),

    remove: (id: string): Promise<void> =>
      unscoped(`/projects/${encodeURIComponent(id)}`, { method: "DELETE" }),
  },

  artifacts: {
    /**
     * Upload one file and get back the reference a message carries.
     *
     * The body is the raw bytes rather than a multipart form: there is one
     * file and no other fields, and the filename — the only thing a form
     * part would have added — rides the query string. `Content-Type` is the
     * browser's claim about the file and nothing more; the server sniffs the
     * bytes and the `mediaType` on the answer is what it found.
     */
    upload: (file: File): Promise<ArtifactRef> =>
      request(`/artifacts?filename=${encodeURIComponent(file.name)}`, {
        method: "POST",
        headers: { "Content-Type": file.type || "application/octet-stream" },
        body: file,
      }),
  },

  sessions: {
    /** Every session a person started, newest first.
     *
     * A run of a workflow or a routine is an ordinary session, so naming one
     * scopes the list to it rather than reading a second endpoint. With neither
     * filter, routine runs are left out — a routine on a timer would otherwise
     * bury the sessions somebody is actually having. */
    list: (filter?: {
      workflow?: string;
      routine?: string;
    }): Promise<ListSessionsResponse> => {
      const q = new URLSearchParams();
      if (filter?.workflow) q.set("workflow", filter.workflow);
      if (filter?.routine) q.set("routine", filter.routine);
      const query = q.toString();
      return request(`/sessions${query ? `?${query}` : ""}`);
    },

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

    /** Send a message to one of a session's agents.
     *
     * `agentId` names who it is for. Not optional in practice: a sub session is an
     * agent, and leaving it out delivered everything typed on a sub session's page to
     * the session's main agent instead. Absent means the main agent, which is
     * what the session page itself wants. */
    send: (
      id: string,
      text: string,
      agentId?: string,
      artifacts: ArtifactRef[] = [],
    ): Promise<SessionAck> =>
      request(
        `/sessions/${encodeURIComponent(id)}/messages` +
          (agentId ? `?aid=${encodeURIComponent(agentId)}` : ""),
        {
          method: "POST",
          // Always sent, even empty. The field is optional server-side for
          // the clients that predate it; this one has no reason to be one
          // of them, and an always-present key is one shape to reason about.
          body: JSON.stringify({ text, artifacts } satisfies SendMessageRequest),
        },
      ),

    /** Remove one agent a session hosts — a subagent's run or a sub session —
     * and everything below it.
     *
     * Not the main agent, which is the session (`remove` deletes that), and
     * not a workflow step, which belongs to its run's log. The server refuses
     * both rather than this guessing which an id names. */
    deleteAgent: (id: string, agentId: string): Promise<Ack> =>
      request(
        `/sessions/${encodeURIComponent(id)}/agents/${encodeURIComponent(agentId)}`,
        { method: "DELETE" },
      ),

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

    /** Cancel one agent's turn. `agentId` is `"main"` or an agent's uuid —
     * there is no session-wide stop, because a session hosts several
     * sessions at once and each has a turn of its own. */
    stop: (id: string, agentId: string): Promise<Ack> =>
      request(
        `/sessions/${encodeURIComponent(id)}/agents/${encodeURIComponent(agentId)}/stop`,
        { method: "POST", body: "{}" },
      ),

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

  inbox: {
    /** A page of the inbox, newest first, with the counts a badge needs. */
    list: (state: InboxScope = "all"): Promise<InboxListResponse> =>
      request(`/inbox?state=${state}`),

    /** Note that these have been opened. */
    markRead: (ids: string[]): Promise<Ack> =>
      request("/inbox/read", {
        method: "POST",
        body: JSON.stringify({ ids } satisfies InboxMessageIds),
      }),

    /** Remove messages. The server declines any question still holding an
     * agent first, so nothing is left parked with its row gone. */
    remove: (ids: string[]): Promise<Ack> =>
      request("/inbox/delete", {
        method: "POST",
        body: JSON.stringify({ ids } satisfies InboxMessageIds),
      }),

    /** Answer a parked question, or say something to the agent behind a
     * notice. The message's own kind decides which, so the caller does not
     * restate what the server already knows. */
    reply: (id: string, text: string): Promise<Ack> =>
      request(`/inbox/${encodeURIComponent(id)}/reply`, {
        method: "POST",
        body: JSON.stringify({ text } satisfies InboxReplyRequest),
      }),
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

    /** Create a session from this preset and queue its first message. */
    invoke: (name: string, body: AgentInvokeRequest): Promise<AgentInvokeResponse> =>
      request(`/agents/${encodeURIComponent(name)}/invoke`, {
        method: "POST",
        body: JSON.stringify(body),
      }),
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

    /** Ask the substrate whether this vendor is usable right now. Nothing is
     * created, and nothing is recorded — a save already proves a configuration
     * before storing it, so this answers the question a stored row cannot: has
     * the token been revoked since. */
    test: (name: string): Promise<RuntimeVendorTestResult> =>
      request(`/runtime-vendors/${encodeURIComponent(name)}/test`, {
        method: "POST",
        body: "{}",
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

    /** The full catalog, backing Settings → Model cards. */
    list: (): Promise<ModelCard[]> => request("/settings/model-cards"),

    create: (body: ModelCardInput): Promise<ModelCard> =>
      request("/settings/model-cards", {
        method: "POST",
        body: JSON.stringify(body),
      }),

    /** Update name/limits; `modelId` is immutable. */
    update: (modelId: string, body: ModelCardUpdate): Promise<ModelCard> =>
      request(`/settings/model-cards/${encodeURIComponent(modelId)}`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),

    remove: (modelId: string): Promise<void> =>
      request(`/settings/model-cards/${encodeURIComponent(modelId)}`, {
        method: "DELETE",
      }),
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
  },

  github: {
    status: (): Promise<GitHubStatus> => request("/github/status"),

    /** Browser navigation target (not fetch) — starts the OAuth flow. */
    authUrl: (): string => scoped(`/github/auth`),

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

  /**
   * The built-in tools this server offers, grouped for selection.
   *
   * Static per build, so a client caches it for the session — see `useTools`.
   * MCP tools are not here: they are chosen by selecting the server.
   */
  tools: {
    // Unscoped: the catalogue is a table compiled into the server binary, the
    // same for every project. Asking for it under a project would be asking a
    // question about the build in a place that answers questions about data.
    catalog: (): Promise<ToolCatalog> => unscoped("/tools"),
  },

  plugins: {
    /** All installed skill bundles (metadata only). */
    list: (): Promise<PluginView[]> => request("/plugins"),

    /**
     * The slash commands horsie answers itself.
     *
     * Not part of `list`: a built-in is offered whether or not any bundle is
     * installed, so folding it into the bundle list would make it disappear
     * from the plainest session there is.
     */
    builtins: (): Promise<CatalogEntryView[]> => request("/builtins"),

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

  /**
   * Plugins authored on this server, whose source is rows in its database
   * rather than a git remote.
   *
   * Separate from `plugins` because the two answer different questions:
   * `plugins` is the library a session picks from — authored bundles appear
   * there too, published — while this is the editable original behind one.
   */
  authored: {
    list: (): Promise<AuthoredPluginView[]> => request("/authored-plugins"),

    get: (name: string): Promise<AuthoredPluginView> =>
      request(`/authored-plugins/${encodeURIComponent(name)}`),

    create: (body: AuthoredPluginWriteInput): Promise<AuthoredPluginView> =>
      request("/authored-plugins", {
        method: "POST",
        body: JSON.stringify(body),
      }),

    /** Deletes the plugin, its skills, and the library entry it published. */
    remove: (name: string): Promise<void> =>
      request(`/authored-plugins/${encodeURIComponent(name)}`, {
        method: "DELETE",
      }),

    getSkill: (name: string, skill: string): Promise<AuthoredSkillView> =>
      request(
        `/authored-plugins/${encodeURIComponent(name)}/skills/${encodeURIComponent(skill)}`,
      ),

    /** Omitted fields keep their current value. */
    writeSkill: (
      name: string,
      skill: string,
      body: {
        description?: string;
        body?: string;
        files?: AuthoredFileView[];
      },
    ): Promise<AuthoredSkillView> =>
      request(
        `/authored-plugins/${encodeURIComponent(name)}/skills/${encodeURIComponent(skill)}`,
        { method: "PUT", body: JSON.stringify(body) },
      ),

    /** Removes the skill; its revisions survive so it can be restored. */
    removeSkill: (name: string, skill: string): Promise<void> =>
      request(
        `/authored-plugins/${encodeURIComponent(name)}/skills/${encodeURIComponent(skill)}`,
        { method: "DELETE" },
      ),

    revisions: (name: string, skill: string): Promise<AuthoredRevisionView[]> =>
      request(
        `/authored-plugins/${encodeURIComponent(name)}/skills/${encodeURIComponent(skill)}/revisions`,
      ),

    restoreSkill: (
      name: string,
      skill: string,
      revision: number,
    ): Promise<AuthoredSkillView> =>
      request(
        `/authored-plugins/${encodeURIComponent(name)}/skills/${encodeURIComponent(skill)}/restore`,
        { method: "POST", body: JSON.stringify({ revision }) },
      ),
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

    /** One server *with* the tools it advertised at its last good connect.
     *
     * A separate call because `list` deliberately carries no tool lists: a few
     * servers with forty tools apiece would be a wall of text on every read,
     * and the tools are only wanted once someone opens one server. */
    get: (name: string): Promise<McpServerDetail> =>
      request(`/mcp/servers/${encodeURIComponent(name)}`),

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
    scoped(`/sessions/${encodeURIComponent(id)}/messages?aid=${encodeURIComponent(agentId)}`),

  /** SSE URL for the global session-status feed. */
  globalEventsUrl: (): string => scoped(`/events`),

  /**
   * Where an artifact's bytes are, as a URL.
   *
   * Usable directly as an `<img src>` or a download href: this app
   * authenticates by cookie, so a browser-issued request for one of these
   * carries the session without any header this code could not have set.
   * Content-addressed, so the URL is safe to cache for ever.
   */
  artifactUrl: (id: string): string =>
    scoped(`/artifacts/${encodeURIComponent(id)}`),
};
