import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { useEffect } from "react";
import { api } from "../api/client";
import type {
  CreateSessionRequest,
  GetSessionResponse,
  GlobalSessionEvent,
  ListSessionsResponse,
} from "../api/types";
import { deriveTitle } from "../lib/format";

export const qk = {
  sessions: ["sessions"] as const,
  session: (id: string) => ["session", id] as const,
  agent: (id: string, agentId: string) => ["session-agent", id, agentId] as const,
};

export function useSessionList() {
  return useQuery({
    queryKey: qk.sessions,
    queryFn: () => api.sessions.list(),
    select: (r: ListSessionsResponse) =>
      [...r.sessions].sort((a, b) => b.createdAt - a.createdAt),
  });
}

export function useSession(id: string | undefined) {
  return useQuery({
    queryKey: id ? qk.session(id) : ["session", "none"],
    queryFn: () => api.sessions.get(id as string),
    enabled: !!id,
    select: (r: GetSessionResponse) => r.session,
  });
}

/** One agent's current values: task list, usage, context-window state. Kept
 * fresh by SSE-driven invalidation (see `useSessionStream`), not polling —
 * every frame that changes one of these is a signal to re-read, never a delta
 * to accumulate. */
export function useAgent(id: string | undefined, agentId: string) {
  return useQuery({
    queryKey: id ? qk.agent(id, agentId) : ["session-agent", "none"],
    queryFn: () => api.sessions.agent(id as string, agentId),
    enabled: !!id,
    select: (r) => r.agent,
  });
}

function applyGlobalStatus(
  client: QueryClient,
  ev: Extract<GlobalSessionEvent, { type: "StatusChanged" }>["value"],
) {
  let matched = false;
  client.setQueryData<ListSessionsResponse>(qk.sessions, (prev) => {
    if (!prev) return prev;
    const sessions = prev.sessions.map((s) => {
      if (s.id !== ev.sessionId) return s;
      matched = true;
      return { ...s, status: ev.status, lastError: ev.reason ?? s.lastError };
    });
    return { sessions };
  });
  // A status change for a session we don't know about yet → refetch the list.
  if (!matched) client.invalidateQueries({ queryKey: qk.sessions });

  client.setQueryData<GetSessionResponse>(
    qk.session(ev.sessionId),
    (prev) =>
      prev
        ? {
            session: {
              ...prev.session,
              status: ev.status,
              lastError: ev.reason ?? prev.session.lastError,
            },
          }
        : prev,
  );
}

function applyGlobalTitle(
  client: QueryClient,
  ev: Extract<GlobalSessionEvent, { type: "TitleChanged" }>["value"],
) {
  let matched = false;
  client.setQueryData<ListSessionsResponse>(qk.sessions, (prev) => {
    if (!prev) return prev;
    const sessions = prev.sessions.map((s) => {
      if (s.id !== ev.sessionId) return s;
      matched = true;
      return { ...s, name: ev.name };
    });
    return { sessions };
  });
  // A title change for a session we don't know about yet → refetch the list.
  if (!matched) client.invalidateQueries({ queryKey: qk.sessions });

  client.setQueryData<GetSessionResponse>(
    qk.session(ev.sessionId),
    (prev) =>
      prev
        ? {
            session: {
              ...prev.session,
              name: ev.name,
            },
          }
        : prev,
  );
}

function applyGlobalEvent(client: QueryClient, ev: GlobalSessionEvent) {
  switch (ev.type) {
    case "StatusChanged":
      applyGlobalStatus(client, ev.value);
      return;
    case "TitleChanged":
      applyGlobalTitle(client, ev.value);
      return;
  }
}

/**
 * Opens the single global SSE feed and keeps the session-list (and any open
 * detail) query caches live as session status or title changes server-side.
 * Mount once, high in the tree.
 */
export function useGlobalSessionFeed() {
  const client = useQueryClient();
  useEffect(() => {
    const es = new EventSource(api.globalEventsUrl());
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        applyGlobalEvent(client, JSON.parse(e.data) as GlobalSessionEvent);
      } catch (err) {
        console.error("failed to parse global session event", err);
      }
    };
    return () => es.close();
  }, [client]);
}

export function useCreateSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateSessionRequest) => api.sessions.create(body),
    onSuccess: () => client.invalidateQueries({ queryKey: qk.sessions }),
  });
}

export function useDeleteSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.sessions.remove(id),
    onSuccess: () => client.invalidateQueries({ queryKey: qk.sessions }),
  });
}

/** Rename a session.
 *
 * The agent's title tool used to be the only writer of a session name, so a
 * session the model never titled kept its raw first message as its name for
 * good. Reported inline by the header that owns the field, hence no global
 * notice. */
export function useRenameSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, name }: { id: string; name: string }) =>
      api.sessions.rename(id, name),
    meta: { inlineError: true },
    onSuccess: (_r, { id }) => {
      client.invalidateQueries({ queryKey: qk.session(id) });
      client.invalidateQueries({ queryKey: qk.sessions });
    },
  });
}

export function useStopSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.sessions.stop(id),
    onSuccess: (_r, id) => {
      client.invalidateQueries({ queryKey: qk.session(id) });
    },
  });
}

/** Give an unnamed session an instant title on its first send, mirroring what
 * the server derives — otherwise the title only appears after a refetch. */
function applyOptimisticTitle(client: QueryClient, id: string, text: string) {
  const detail = client.getQueryData<GetSessionResponse>(qk.session(id));
  if (detail?.session.name) return;
  const list = client.getQueryData<ListSessionsResponse>(qk.sessions);
  if (list?.sessions.find((s) => s.id === id)?.name) return;
  const title = deriveTitle(text);
  if (!title) return;

  client.setQueryData<GetSessionResponse>(qk.session(id), (prev) =>
    prev ? { session: { ...prev.session, name: title } } : prev,
  );
  client.setQueryData<ListSessionsResponse>(qk.sessions, (prev) =>
    prev
      ? {
          sessions: prev.sessions.map((s) =>
            s.id === id ? { ...s, name: title } : s,
          ),
        }
      : prev,
  );
}

/** Answer a session's pending asks. All of them, in one request. */
export function useAnswerAsks() {
  return useMutation({
    mutationFn: ({
      id,
      agentId,
      answers,
    }: {
      id: string;
      agentId: string;
      answers: { toolCallId: string; text: string }[];
    }) => api.sessions.answerAsks(id, agentId, answers),
  });
}

export function useSendMessage() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, text }: { id: string; text: string }) =>
      api.sessions.send(id, text),
    // The session view renders a failed send inline, right above the composer
    // that produced it. Without this the global notice reported it a second
    // time, in a corner, in the same words.
    meta: { inlineError: true },
    onMutate: ({ id, text }) => applyOptimisticTitle(client, id, text),
  });
}
