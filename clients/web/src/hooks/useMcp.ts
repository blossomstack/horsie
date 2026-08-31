import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { McpServerInput } from "../api/types";

export const mcpKeys = {
  servers: ["mcp", "servers"] as const,
  // A prefix of `servers`, deliberately: a test refreshes the stored tool
  // catalogue, so invalidating the list has to drop the open tool lists with
  // it or they go on showing what the server used to offer.
  server: (name: string) => ["mcp", "servers", name] as const,
};

/** The configured MCP servers (redacted). */
export function useMcpServers() {
  return useQuery({
    queryKey: mcpKeys.servers,
    queryFn: () => api.mcp.list().then((r) => r.servers),
  });
}

/**
 * One server with its remembered tools, fetched only when someone asks to see
 * them. `enabled` is what makes this lazy: a settings row or a picker mounts
 * for every configured server, and fetching every catalogue up front would
 * cost a request per server to show something nobody has opened.
 */
export function useMcpServer(name: string, enabled = true) {
  return useQuery({
    queryKey: mcpKeys.server(name),
    queryFn: () => api.mcp.get(name),
    enabled: enabled && name !== "",
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
