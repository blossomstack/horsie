import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, ApiRequestError, setCurrentProject } from "./client";

// Every scoped call carries the project, so a suite that never sets one is
// testing a state no browser is ever in. `ProjectScope` does this from the URL.
setCurrentProject("p1");

/**
 * The error path only. Every one of these cases reached a user as a bare
 * status line, because `request` called `res.json()` —
 * which *throws* on a non-JSON body — and kept the status line as the message.
 */
describe("agent invocation", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    vi.stubGlobal("fetch", fetchMock);
    fetchMock.mockReset();
  });
  afterEach(() => vi.unstubAllGlobals());

  it("posts a first message and environment to the selected agent", async () => {
    fetchMock.mockResolvedValue(
      new Response(JSON.stringify({ session: { id: "s1" } }), { status: 200 }),
    );

    await api.agents.invoke("reviewer", {
      message: "Review this change",
      environment: { type: "Runtime", value: { vendor: "local" } },
    });

    expect(fetchMock).toHaveBeenCalledWith("/api/p/p1/agents/reviewer/invoke", {
      headers: { "Content-Type": "application/json" },
      method: "POST",
      body: JSON.stringify({
        message: "Review this change",
        environment: { type: "Runtime", value: { vendor: "local" } },
      }),
    });
  });
});

describe("request error reporting", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    vi.stubGlobal("fetch", fetchMock);
    fetchMock.mockReset();
  });
  afterEach(() => vi.unstubAllGlobals());

  const reply = (init: {
    status: number;
    statusText?: string;
    body: string;
  }) =>
    fetchMock.mockResolvedValue(
      new Response(init.body, {
        status: init.status,
        statusText: init.statusText ?? "",
      }),
    );

  const failure = async (): Promise<ApiRequestError> => {
    try {
      await api.auth.status();
      throw new Error("expected the request to reject");
    } catch (e) {
      if (!(e instanceof ApiRequestError)) throw e;
      return e;
    }
  };

  it("uses horsie's own {code,message} envelope", async () => {
    reply({
      status: 409,
      body: JSON.stringify({
        code: "agent_in_use",
        message: "routine 'nightly' uses this agent",
      }),
    });
    const e = await failure();
    expect(e.code).toBe("agent_in_use");
    expect(e.message).toBe("routine 'nightly' uses this agent");
  });

  it("keeps a text/plain body — this is what axum's rejections send", async () => {
    // A real one, verbatim. The user saw `422 ` instead.
    reply({
      status: 422,
      body: "provision[0]: missing field `name` at line 1 column 63",
    });
    const e = await failure();
    expect(e.message).toBe(
      "provision[0]: missing field `name` at line 1 column 63",
    );
    expect(e.status).toBe(422);
  });

  it("falls back to the status when the body is empty", async () => {
    // `statusText` is absent over HTTP/2, which is how a bare `405 ` with a
    // trailing space reached the screen.
    reply({ status: 405, body: "" });
    const e = await failure();
    expect(e.message).toBe("405");
  });

  it("prefers statusText when there is one and no body", async () => {
    reply({ status: 404, statusText: "Not Found", body: "" });
    const e = await failure();
    expect(e.message).toBe("404 Not Found");
  });

  it("does not mistake a JSON body without a message for one", async () => {
    reply({ status: 500, body: JSON.stringify({ nope: true }) });
    const e = await failure();
    expect(e.message).toBe('{"nope":true}');
  });
});

/**
 * A scoped call with no project is a routing bug, and it says so rather than
 * quietly asking for `/api/agents` — which the server answers with a 404 that
 * reads like an empty account.
 */
describe("the project prefix", () => {
  it("refuses a scoped call before a project is known", async () => {
    setCurrentProject(null as unknown as string);
    await expect(api.agents.list()).rejects.toThrow(/No project selected/);
    setCurrentProject("p1");
  });

  it("leaves the credential routes unprefixed", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response("{}"));
    vi.stubGlobal("fetch", fetchMock);
    await api.auth.status();
    expect(fetchMock.mock.calls[0][0]).toBe("/api/auth/status");
    vi.unstubAllGlobals();
  });

  /**
   * The deployment-wide routes, which are not a project's to answer.
   *
   * Asking for one under a project is a 404 the UI renders as an empty
   * feature — the tool picker came up with no groups at all, which reads as
   * "this server offers no tools" rather than as a wrong URL.
   */
  it("leaves the deployment-wide routes unprefixed", async () => {
    // A fresh `Response` per call: a body can only be read once.
    const fetchMock = vi.fn().mockImplementation(() => new Response("{}"));
    vi.stubGlobal("fetch", fetchMock);
    for (const [call, expected] of [
      [() => api.tools.catalog(), "/api/tools"],
      [() => api.health(), "/api/health"],
      [() => api.projects.list(), "/api/projects"],
    ] as const) {
      fetchMock.mockClear();
      await call();
      expect(fetchMock.mock.calls[0][0]).toBe(expected);
    }
    vi.unstubAllGlobals();
  });
});
