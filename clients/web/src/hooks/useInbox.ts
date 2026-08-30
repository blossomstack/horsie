import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type InboxScope } from "../api/client";

export const inboxKeys = {
  all: ["inbox"] as const,
  list: (scope: InboxScope) => ["inbox", scope] as const,
};

/**
 * One slice of the project's inbox, with the counts the badge reads.
 *
 * No feed of its own, deliberately. An ask is written while a turn is parked
 * and a notice while one runs, and both move the session list the global feed
 * already publishes — so `useGlobalSessionFeed` invalidates this on every
 * frame, which is a second stream's worth of liveness for no second stream.
 * The interval is the floor under the case that frame never comes: a notice
 * from an agent whose session row says exactly what it said before.
 */
export function useInbox(scope: InboxScope = "all") {
  return useQuery({
    queryKey: inboxKeys.list(scope),
    queryFn: () => api.inbox.list(scope),
    refetchInterval: 20_000,
  });
}

/** Note that these have been opened. */
export function useMarkInboxRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (ids: string[]) => api.inbox.markRead(ids),
    onSuccess: () => qc.invalidateQueries({ queryKey: inboxKeys.all }),
  });
}

/** Remove messages. Any question still holding an agent is declined by the
 * server first, so this changes what an agent is doing as well as what the
 * list shows. */
export function useDeleteInboxMessages() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (ids: string[]) => api.inbox.remove(ids),
    onSuccess: () => qc.invalidateQueries({ queryKey: inboxKeys.all }),
  });
}

/** Answer a parked question, or say something to the agent behind a notice.
 *
 * Invalidated on failure too: a 409 means the question was settled somewhere
 * else, and the row that still reads as open is the reason the reply was
 * typed. */
export function useReplyToInboxMessage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, text }: { id: string; text: string }) =>
      api.inbox.reply(id, text),
    onSettled: () => qc.invalidateQueries({ queryKey: inboxKeys.all }),
  });
}
