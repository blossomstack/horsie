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

export const qk = {
  sessions: ["sessions"] as const,
  session: (id: string) => ["session", id] as const,
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

function applyGlobalEvent(client: QueryClient, ev: GlobalSessionEvent) {
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

/**
 * Opens the single global SSE feed and keeps the session-list (and any open
 * detail) query caches live as statuses change server-side. Mount once, high
 * in the tree.
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

export function useStopSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.sessions.stop(id),
    onSuccess: (_r, id) => {
      client.invalidateQueries({ queryKey: qk.session(id) });
    },
  });
}

export function useSendMessage() {
  return useMutation({
    mutationFn: ({ id, text }: { id: string; text: string }) =>
      api.sessions.send(id, text),
  });
}
