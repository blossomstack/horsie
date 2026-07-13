import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { McpServerInput } from "../api/types";

export const mcpKeys = {
  servers: ["mcp", "servers"] as const,
};

/** The configured MCP servers (redacted). */
export function useMcpServers() {
  return useQuery({
    queryKey: mcpKeys.servers,
    queryFn: () => api.mcp.list().then((r) => r.servers),
  });
}

export function useUpsertMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: McpServerInput }) =>
      api.mcp.upsert(name, body),
    onSuccess: () => qc.invalidateQueries({ queryKey: mcpKeys.servers }),
  });
}

export function useDeleteMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.mcp.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: mcpKeys.servers }),
  });
}

/** Connect/smoke-test a server; refreshes the list with the recorded outcome. */
export function useTestMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.mcp.test(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: mcpKeys.servers }),
  });
}

/** Start the OAuth flow for a server; returns the authorize URL to redirect to. */
export function useConnectMcpServer() {
  return useMutation({
    mutationFn: (name: string) => api.mcp.connect(name),
  });
}
