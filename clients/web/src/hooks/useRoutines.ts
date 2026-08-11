import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { RoutineInput } from "../api/types";
import { qk } from "./useSessions";

export const routineKeys = {
  all: ["routines"] as const,
  one: (name: string) => ["routines", name] as const,
  sessions: (name: string) => ["routines", name, "sessions"] as const,
};

/** All routines. */
export function useRoutines() {
  return useQuery({
    queryKey: routineKeys.all,
    queryFn: () => api.routines.list(),
  });
}

export function useRoutine(name: string | undefined) {
  return useQuery({
    queryKey: name ? routineKeys.one(name) : ["routines", "none"],
    queryFn: () => api.routines.get(name as string),
    enabled: !!name,
  });
}

/** A routine's runs, newest first. Refetched on a timer while the page is
 * open: a run's status lives in the session, not the routine. */
export function useRoutineSessions(name: string | undefined) {
  return useQuery({
    queryKey: name ? routineKeys.sessions(name) : ["routines", "none", "sessions"],
    queryFn: () => api.sessions.list({ routine: name as string }),
    enabled: !!name,
    refetchInterval: 5_000,
    select: (r) => r.sessions,
  });
}

export function useCreateRoutine() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: RoutineInput) => api.routines.create(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: routineKeys.all }),
  });
}

export function useUpdateRoutine() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: RoutineInput }) =>
      api.routines.update(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: routineKeys.all });
      qc.invalidateQueries({ queryKey: routineKeys.one(name) });
    },
  });
}

/** Deleting a routine also deletes its runs, so the session list is stale
 * afterwards too. */
export function useDeleteRoutine() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.routines.remove(name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: routineKeys.all });
      qc.invalidateQueries({ queryKey: qk.sessions });
    },
  });
}

export function useRunRoutine() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.routines.run(name),
    onSuccess: (_r, name) => {
      qc.invalidateQueries({ queryKey: routineKeys.one(name) });
      qc.invalidateQueries({ queryKey: routineKeys.sessions(name) });
    },
  });
}
