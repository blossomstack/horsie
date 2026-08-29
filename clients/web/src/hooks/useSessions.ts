import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { useEffect } from "react";
import { api } from "../api/client";
import { deriveTitle } from "../lib/format";
import type {
  ArtifactRef,
  CreateSessionRequest,
  GetSessionResponse,
  ListSessionsResponse,
} from "../api/types";

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

/**
 * Take the session list a frame carries as the current truth.
 *
 * Replaces three per-field patchers — status, title, sub sessions — which existed
 * because a frame used to describe one changed field. A frame is now the whole
 * list, so there is nothing to reconcile: a reader that missed a frame is
 * corrected by the next one rather than left holding a half-applied delta.
 *
 * Any open detail query is refreshed from the same list, so a session's own
 * page cannot disagree with the row for it in the sidebar.
 */
export function applySessionList(
  client: QueryClient,
  next: ListSessionsResponse,
) {
  client.setQueryData<ListSessionsResponse>(qk.sessions, next);
  for (const session of next.sessions) {
    client.setQueryData<GetSessionResponse>(qk.session(session.id), (prev) =>
      prev ? { session: { ...prev.session, ...session } } : prev,
    );
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
        applySessionList(client, JSON.parse(e.data) as ListSessionsResponse);
      } catch (err) {
        console.error("failed to parse the session list", err);
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

/** Cancel one agent's turn. The agent is named, never implied: on a sub session's page
 * the unscoped stop cancelled the main agent's turn instead — or, once a sub session
 * was what was running, nothing at all. */
export function useStopAgent() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, agentId }: { id: string; agentId: string }) =>
      api.sessions.stop(id, agentId),
    onSuccess: (_r, { id }) => {
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
    mutationFn: ({
      id,
      text,
      agentId,
      artifacts,
    }: {
      id: string;
      text: string;
      agentId?: string;
      /** What the composer already uploaded. Ids only — the bytes went up on
       * attach, which is why sending stays a small JSON request. */
      artifacts?: ArtifactRef[];
    }) => api.sessions.send(id, text, agentId, artifacts),
    // The session view renders a failed send inline, right above the composer
    // that produced it. Without this the global notice reported it a second
    // time, in a corner, in the same words.
    meta: { inlineError: true },
    // Only the session's own first message names it. A message to a sub session is
    // that sub session's business, and titling the session from it would rename the
    // session somebody branched *away* from.
    onMutate: ({ id, text, agentId }) =>
      agentId ? undefined : applyOptimisticTitle(client, id, text),
  });
}

/** Remove one agent a session hosts — a subagent's run or a sub session — and
 * everything below it. Nothing removes either on its own, so this is the only
 * way. */
export function useDeleteAgent() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, agentId }: { id: string; agentId: string }) =>
      api.sessions.deleteAgent(id, agentId),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ["sessions"] });
    },
  });
}
