import {
  type QueryClient,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../api/client";
import type { AuthoredFileView, AuthoredPluginWriteInput } from "../api/types";
import { pluginsKey } from "./usePlugins";

export const authoredKey = ["authored-plugins"] as const;

/** Revisions are per skill, so the key carries both halves of its address. */
export const revisionsKey = (plugin: string, skill: string) =>
  ["authored-plugins", plugin, skill, "revisions"] as const;

/**
 * Every write re-renders the published bundle, so the library list moves with
 * the authored one. Invalidating only the authored side would leave the skills
 * page showing a stale skill count for a bundle that just changed.
 */
function invalidateBoth(client: QueryClient) {
  void client.invalidateQueries({ queryKey: authoredKey });
  void client.invalidateQueries({ queryKey: pluginsKey });
}

export function useAuthoredPlugins() {
  return useQuery({ queryKey: authoredKey, queryFn: () => api.authored.list() });
}

export function useSkillRevisions(
  plugin: string,
  skill: string,
  enabled: boolean,
) {
  return useQuery({
    queryKey: revisionsKey(plugin, skill),
    queryFn: () => api.authored.revisions(plugin, skill),
    enabled,
  });
}

export function useCreateAuthoredPlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: AuthoredPluginWriteInput) => api.authored.create(body),
    onSuccess: () => invalidateBoth(client),
  });
}

export function useRemoveAuthoredPlugin() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.authored.remove(name),
    onSuccess: () => invalidateBoth(client),
  });
}

export function useWriteSkill() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: {
      plugin: string;
      skill: string;
      description?: string;
      body?: string;
      files?: AuthoredFileView[];
    }) =>
      api.authored.writeSkill(args.plugin, args.skill, {
        description: args.description,
        body: args.body,
        files: args.files,
      }),
    onSuccess: (_data, args) => {
      invalidateBoth(client);
      void client.invalidateQueries({
        queryKey: revisionsKey(args.plugin, args.skill),
      });
    },
  });
}

export function useRemoveSkill() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: { plugin: string; skill: string }) =>
      api.authored.removeSkill(args.plugin, args.skill),
    onSuccess: () => invalidateBoth(client),
  });
}

export function useRestoreSkill() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: { plugin: string; skill: string; revision: number }) =>
      api.authored.restoreSkill(args.plugin, args.skill, args.revision),
    onSuccess: (_data, args) => {
      invalidateBoth(client);
      void client.invalidateQueries({
        queryKey: revisionsKey(args.plugin, args.skill),
      });
    },
  });
}
