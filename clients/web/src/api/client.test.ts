import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, ApiRequestError } from "./client";

/**
 * The error path only. Every one of these cases reached a user as a bare
 * status line, because `request` called `res.json()` —
 * which *throws* on a non-JSON body — and kept the status line as the message.
 */
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
