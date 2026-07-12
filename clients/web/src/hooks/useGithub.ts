import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { GitHubAppConfigInput } from "../api/types";

export const githubKeys = {
  status: ["github", "status"] as const,
  appConfig: ["github", "app-config"] as const,
  repos: ["github", "repos"] as const,
};

export function useGithubStatus() {
  return useQuery({ queryKey: githubKeys.status, queryFn: api.github.status });
}

export function useGithubAppConfig() {
  return useQuery({
    queryKey: githubKeys.appConfig,
    queryFn: api.github.appConfig,
  });
}

export function useSaveGithubAppConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: GitHubAppConfigInput) => api.github.saveAppConfig(body),
    onSuccess: (view) => {
      qc.setQueryData(githubKeys.appConfig, view);
      qc.invalidateQueries({ queryKey: githubKeys.status });
    },
  });
}

export function useGithubDisconnect() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.github.disconnect(),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: githubKeys.status });
      qc.invalidateQueries({ queryKey: githubKeys.repos });
    },
  });
}

/** Repo list for the picker; only fetched while the picker is open/connected. */
export function useGithubRepos(enabled: boolean) {
  return useQuery({
    queryKey: githubKeys.repos,
    queryFn: () => api.github.repos(),
    enabled,
    staleTime: 5 * 60 * 1000, // server caches 5 min too
  });
}
