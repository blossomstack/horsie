import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import type { SubSessionView, ListSessionsResponse, SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { applySessionList, qk } from "./useSessions";

vi.mock("../api/client", () => ({ api: { sessions: {} } }));

function subSession(id: string, title?: string): SubSessionView {
  return {
    id,
    title,
    status: "idle",
    createdAtMs: 5,
    lastActivityMs: 5,
  };
}

function session(id: string, over: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: [],
    subSessions: [],
    ...over,
  };
}

function list(...sessions: SessionSummary[]): ListSessionsResponse {
  return { sessions };
}

describe("a session-list frame", () => {
  it("is taken as the whole truth, not merged into what is held", () => {
    // The property that makes a missed frame harmless: whatever arrives is the
    // list, so a reader cannot end up holding a half-applied change.
    const client = new QueryClient();
    client.setQueryData(qk.sessions, list(session("s1"), session("s2")));

    applySessionList(client, list(session("s2", { name: "renamed" })));

    const held = client.getQueryData<ListSessionsResponse>(qk.sessions);
    expect(held?.sessions.map((s) => s.id)).toEqual(["s2"]);
    expect(held?.sessions[0].name).toBe("renamed");
  });

  it("carries a new subSession without a refetch", () => {
    // What the three per-field frames existed to do, now falling out of the
    // list itself: no invalidation, so no round trip.
    const client = new QueryClient();
    client.setQueryData(qk.sessions, list(session("s1")));
    const invalidate = vi.spyOn(client, "invalidateQueries");

    applySessionList(
      client,
      list(session("s1", { subSessions: [subSession("f1", "The other direction")] })),
    );

    const held = client.getQueryData<ListSessionsResponse>(qk.sessions);
    expect(held?.sessions[0].subSessions).toEqual([subSession("f1", "The other direction")]);
    expect(invalidate).not.toHaveBeenCalled();
  });

  it("carries a session it has never seen", () => {
    // Previously this needed a refetch, because a delta about an unknown
    // session had nothing to attach to.
    const client = new QueryClient();
    client.setQueryData(qk.sessions, list());
    const invalidate = vi.spyOn(client, "invalidateQueries");

    applySessionList(client, list(session("brand-new")));

    const held = client.getQueryData<ListSessionsResponse>(qk.sessions);
    expect(held?.sessions.map((s) => s.id)).toEqual(["brand-new"]);
    expect(invalidate).not.toHaveBeenCalled();
  });

  it("updates an open session detail too", () => {
    // So a session's own page cannot disagree with its row in the sidebar.
    const client = new QueryClient();
    client.setQueryData(qk.sessions, list(session("s1")));
    client.setQueryData(qk.session("s1"), { session: session("s1") });

    applySessionList(client, list(session("s1", { subSessions: [subSession("f1")] })));

    const detail = client.getQueryData<{ session: SessionSummary }>(
      qk.session("s1"),
    );
    expect(detail?.session.subSessions).toEqual([subSession("f1")]);
  });

  it("leaves a detail it holds nothing for alone", () => {
    // Writing one would put a session into the cache that no query asked for.
    const client = new QueryClient();
    applySessionList(client, list(session("s1")));

    expect(client.getQueryData(qk.session("s1"))).toBeUndefined();
  });
});
