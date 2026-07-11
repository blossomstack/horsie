import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { SettingsUpdate, SettingsView } from "../api/types";

export const settingsKey = ["settings"] as const;

/** The server's runtime-editable configuration (providers, models, vendors). */
export function useSettings() {
  return useQuery({ queryKey: settingsKey, queryFn: () => api.config.get() });
}

/** Persist + live-apply a settings update, seeding the cache with the result. */
export function useUpdateSettings() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: SettingsUpdate) => api.config.update(body),
    onSuccess: (view: SettingsView) => client.setQueryData(settingsKey, view),
  });
}
