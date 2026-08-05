import {
  type QueryClient,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../api/client";
import type { PluginDefaultInput, PluginInstallInput } from "../api/types";

export const pluginsKey = ["plugins"] as const;
export const marketplacesKey = ["marketplaces"] as const;

/** The two lists move together: installing an entry flips its `installed` flag
 * in the catalogue, and removing the bundle flips it back. Invalidating only
 * one leaves the picker offering something already in the library. */
function invalidateBoth(client: QueryClient) {
  void client.invalidateQueries({ queryKey: pluginsKey });
  void client.invalidateQueries({ queryKey: marketplacesKey });
}

/** The installed skill bundles (metadata only). */
export function usePlugins() {
  return useQuery({ queryKey: pluginsKey, queryFn: () => api.plugins.list() });
}

/** Install a bundle from a git repo, then refresh the list. */
export function useInstallPlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: PluginInstallInput) => api.plugins.install(body),
    onSuccess: () => invalidateBoth(client),
  });
}

/** Re-clone a bundle at its ref, then refresh the list. */
export function useUpdatePlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.plugins.update(name),
    onSuccess: () => invalidateBoth(client),
  });
}

/** Toggle whether a bundle is pre-selected for new sessions. */
export function useSetPluginDefault() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ name, enabledDefault }: { name: string } & PluginDefaultInput) =>
      api.plugins.setDefault(name, { enabledDefault }),
    onSuccess: () => invalidateBoth(client),
  });
}

/** Uninstall a bundle, then refresh the list. */
export function useRemovePlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.plugins.remove(name),
    onSuccess: () => invalidateBoth(client),
  });
}

/** Registered marketplaces, each carrying its cached catalogue. */
export function useMarketplaces() {
  return useQuery({
    queryKey: marketplacesKey,
    queryFn: () => api.marketplaces.list(),
  });
}

/** Re-clone a source's index to pick up plugins published since it was added. */
export function useRefreshMarketplace() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.marketplaces.refresh(name),
    onSuccess: () => invalidateBoth(client),
  });
}

/** Drop a source. Bundles installed from it stay installed. */
export function useRemoveMarketplace() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.marketplaces.remove(name),
    onSuccess: () => invalidateBoth(client),
  });
}
