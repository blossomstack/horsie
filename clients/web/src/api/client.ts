import type {
  ApiError,
  CreateSessionRequest,
  CreateSessionResponse,
  GetSessionResponse,
  HistoryPage,
  GitHubAppConfigInput,
  GitHubAppConfigView,
  GitHubBranchList,
  GitHubRepoList,
  GitHubStatus,
  ListSessionsResponse,
  McpAuthorizeUrl,
  McpConnectResult,
  McpServerInput,
  McpServerList,
  McpServerView,
  PluginDefaultInput,
  PluginInstallInput,
  PluginView,
  SessionAck,
  SettingsUpdate,
  SettingsView,
  VendorTestResult,
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
    throw new ApiRequestError(res.status, code, message);
  }

  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export const api = {
  health: (): Promise<{ ok: boolean }> => request("/health"),

  sessions: {
    list: (): Promise<ListSessionsResponse> => request("/sessions"),

    get: (id: string): Promise<GetSessionResponse> =>
      request(`/sessions/${encodeURIComponent(id)}`),

    /** A window of conversation history from the agent's in-memory state.
     * No `before` requests the latest (tail) page, which also carries the
     * current task list + cumulative usage. */
    history: (
      id: string,
      opts: { before?: string; limit?: number } = {},
    ): Promise<HistoryPage> => {
      const q = new URLSearchParams();
      if (opts.before) q.set("before", opts.before);
      if (opts.limit) q.set("limit", String(opts.limit));
      const qs = q.toString();
      return request(
        `/sessions/${encodeURIComponent(id)}/history${qs ? `?${qs}` : ""}`,
      );
    },

    create: (body: CreateSessionRequest): Promise<CreateSessionResponse> =>
      request("/sessions", { method: "POST", body: JSON.stringify(body) }),

    remove: (id: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}`, { method: "DELETE" }),

    send: (id: string, text: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/messages`, {
        method: "POST",
        body: JSON.stringify({ text }),
      }),

    stop: (id: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/stop`, {
        method: "POST",
        body: "{}",
      }),
  },

  config: {
    /** The current redacted settings (providers, models, vendors, deployment info). */
    get: (): Promise<SettingsView> => request("/config"),

    /** Persist + live-apply a settings update; returns the new view. */
    update: (body: SettingsUpdate): Promise<SettingsView> =>
      request("/config", { method: "PUT", body: JSON.stringify(body) }),

    /** On-demand reachability + token check for a vendor (velos only); never mutates settings. */
    testVendor: (name: string): Promise<VendorTestResult> =>
      request(`/config/vendors/${encodeURIComponent(name)}/test`, {
        method: "POST",
        body: "{}",
      }),
  },

  github: {
    status: (): Promise<GitHubStatus> => request("/github/status"),

    /** Browser navigation target (not fetch) — starts the OAuth flow. */
    authUrl: (): string => `${BASE}/github/auth`,

    appConfig: (): Promise<GitHubAppConfigView> => request("/github/app-config"),

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
      request(
        `/github/repos/branches?repo=${encodeURIComponent(repo)}`,
      ),
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

  /** SSE URL for a single session's event stream. `live` streams only events
   * after connect (skipping journal replay) — the paginating client backfills
   * history via `sessions.history` and uses this for live updates. */
  sessionEventsUrl: (id: string, opts: { live?: boolean } = {}): string =>
    `${BASE}/sessions/${encodeURIComponent(id)}/events${opts.live ? "?live=1" : ""}`,

  /** SSE URL for the global session-status feed. */
  globalEventsUrl: (): string => `${BASE}/events`,
};
