import type {
  ApiError,
  CreateSessionRequest,
  CreateSessionResponse,
  GetSessionResponse,
  ListSessionsResponse,
  SessionAck,
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
  },

  /** SSE URL for a single session's durable + live event stream. */
  sessionEventsUrl: (id: string): string =>
    `${BASE}/sessions/${encodeURIComponent(id)}/events`,

  /** SSE URL for the global session-status feed. */
  globalEventsUrl: (): string => `${BASE}/events`,
};
