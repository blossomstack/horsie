import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, getCurrentProject } from "../api/client";
import type { ProjectView } from "../api/types";

export const projectKeys = {
  all: ["projects"] as const,
};

/**
 * This account's projects.
 *
 * Survives `ProjectScope`'s cache drop by being refetched rather than kept: the
 * list belongs to the account, not to a project, but it is cheap and a stale
 * one is only ever a missing switcher entry.
 */
export function useProjects() {
  return useQuery({
    queryKey: projectKeys.all,
    queryFn: () => api.projects.list(),
    staleTime: 30_000,
  });
}

/**
 * The project this tab is in, as a full row rather than the bare id in the URL.
 *
 * From the API client rather than from `useParams`: the project is the router's
 * *basename*, so it is not a route parameter any component can read.
 *
 * `undefined` while the list is loading, and on the account-level pages, which
 * are outside any project.
 */
export function useCurrentProject(): ProjectView | undefined {
  const id = getCurrentProject();
  const { data } = useProjects();
  return data?.find((p) => p.id === id);
}

export function useCreateProject() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.projects.create(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: projectKeys.all }),
  });
}

export function useRenameProject() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, name }: { id: string; name: string }) =>
      api.projects.rename(id, name),
    onSuccess: () => qc.invalidateQueries({ queryKey: projectKeys.all }),
  });
}

export function useDeleteProject() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.projects.remove(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: projectKeys.all }),
  });
}
