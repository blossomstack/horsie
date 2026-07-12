import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { PluginDefaultInput, PluginInstallInput } from "../api/types";

export const pluginsKey = ["plugins"] as const;

/** The installed skill bundles (metadata only). */
export function usePlugins() {
  return useQuery({ queryKey: pluginsKey, queryFn: () => api.plugins.list() });
}

/** Install a bundle from a git repo, then refresh the list. */
export function useInstallPlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: PluginInstallInput) => api.plugins.install(body),
    onSuccess: () => client.invalidateQueries({ queryKey: pluginsKey }),
  });
}

/** Re-clone a bundle at its ref, then refresh the list. */
export function useUpdatePlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.plugins.update(name),
    onSuccess: () => client.invalidateQueries({ queryKey: pluginsKey }),
  });
}

/** Toggle whether a bundle is pre-selected for new sessions. */
export function useSetPluginDefault() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ name, enabledDefault }: { name: string } & PluginDefaultInput) =>
      api.plugins.setDefault(name, { enabledDefault }),
    onSuccess: () => client.invalidateQueries({ queryKey: pluginsKey }),
  });
}

/** Uninstall a bundle, then refresh the list. */
export function useRemovePlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.plugins.remove(name),
    onSuccess: () => client.invalidateQueries({ queryKey: pluginsKey }),
  });
}
