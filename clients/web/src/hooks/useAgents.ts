import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { AgentInvokeRequest, AgentPresetInput } from "../api/types";

export const agentKeys = {
  all: ["agents"] as const,
  one: (name: string) => ["agents", name] as const,
};

/** All agent presets. */
export function useAgents() {
  return useQuery({ queryKey: agentKeys.all, queryFn: () => api.agents.list() });
}

export function useAgent(name: string | undefined) {
  return useQuery({
    queryKey: name ? agentKeys.one(name) : ["agents", "none"],
    queryFn: () => api.agents.get(name as string),
    enabled: !!name,
  });
}

export function useCreateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: AgentPresetInput) => api.agents.create(body),
    onSuccess: () => qc.invalidateQueries({ queryKey: agentKeys.all }),
  });
}

export function useUpdateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: AgentPresetInput }) =>
      api.agents.update(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: agentKeys.all });
      qc.invalidateQueries({ queryKey: agentKeys.one(name) });
    },
  });
}

export function useInvokeAgent() {
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: AgentInvokeRequest }) =>
      api.agents.invoke(name, body),
  });
}

export function useDeleteAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.agents.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: agentKeys.all }),
  });
}
