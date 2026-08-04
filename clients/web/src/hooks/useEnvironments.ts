import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { EnvironmentInput } from "../api/types";

export const environmentKeys = {
  all: ["environments"] as const,
  one: (name: string) => ["environments", name] as const,
};

/** All environments. */
export function useEnvironments() {
  return useQuery({
    queryKey: environmentKeys.all,
    queryFn: () => api.environments.list(),
  });
}

export function useEnvironment(name: string | undefined) {
  return useQuery({
    queryKey: name ? environmentKeys.one(name) : ["environments", "none"],
    queryFn: () => api.environments.get(name as string),
    enabled: !!name,
  });
}

export function useCreateEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: EnvironmentInput) => api.environments.create(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: environmentKeys.all }),
  });
}

export function useUpdateEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: EnvironmentInput }) =>
      api.environments.update(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: environmentKeys.all });
      qc.invalidateQueries({ queryKey: environmentKeys.one(name) });
    },
  });
}

export function useDeleteEnvironment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.environments.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: environmentKeys.all }),
  });
}
