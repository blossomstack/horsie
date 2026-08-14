import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import type { ForkView, ListSessionsResponse } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { applyGlobalEvent, qk } from "./useSessions";

vi.mock("../api/client", () => ({ api: { sessions: {} } }));

function fork(id: string, title?: string): ForkView {
  return {
    id,
    title,
    status: "idle",
    createdAtMs: 5,
    lastActivityMs: 5,
  };
}

function listWith(forks: ForkView[]): ListSessionsResponse {
  return {
    sessions: [
      {
        id: "s1",
        status: SessionStatusKind.Idle,
        createdAt: 1,
        annotations: [],
        forks,
      },
    ],
  };
}

describe("a ForksChanged frame", () => {
  it("puts a new fork into the session list without a refetch", () => {
    const client = new QueryClient();
    client.setQueryData(qk.sessions, listWith([]));
    const invalidate = vi.spyOn(client, "invalidateQueries");

    applyGlobalEvent(client, {
      type: "ForksChanged",
      value: { sessionId: "s1", forks: [fork("f1", "The other direction")] },
    });

    const list = client.getQueryData<ListSessionsResponse>(qk.sessions);
    expect(list?.sessions[0].forks).toEqual([fork("f1", "The other direction")]);
    expect(invalidate).not.toHaveBeenCalled();
  });

  it("replaces the roster rather than merging into it", () => {
    const client = new QueryClient();
    client.setQueryData(qk.sessions, listWith([fork("f1"), fork("f2")]));

    applyGlobalEvent(client, {
      type: "ForksChanged",
      value: { sessionId: "s1", forks: [fork("f2")] },
    });

    const list = client.getQueryData<ListSessionsResponse>(qk.sessions);
    expect(list?.sessions[0].forks.map((f) => f.id)).toEqual(["f2"]);
  });

  it("refetches the list when the session is not in it yet", () => {
    const client = new QueryClient();
    client.setQueryData(qk.sessions, listWith([]));
    const invalidate = vi.spyOn(client, "invalidateQueries");

    applyGlobalEvent(client, {
      type: "ForksChanged",
      value: { sessionId: "unknown", forks: [fork("f1")] },
    });

    expect(invalidate).toHaveBeenCalledWith({ queryKey: qk.sessions });
  });

  it("updates an open session detail too", () => {
    const client = new QueryClient();
    client.setQueryData(qk.sessions, listWith([]));
    client.setQueryData(qk.session("s1"), {
      session: {
        id: "s1",
        status: SessionStatusKind.Idle,
        createdAt: 1,
        annotations: [],
        forks: [],
      },
    });

    applyGlobalEvent(client, {
      type: "ForksChanged",
      value: { sessionId: "s1", forks: [fork("f1")] },
    });

    const detail = client.getQueryData<{
      session: { forks: ForkView[] };
    }>(qk.session("s1"));
    expect(detail?.session.forks.map((f) => f.id)).toEqual(["f1"]);
  });
});
