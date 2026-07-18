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

/**
 * On-demand connection check for a configured vendor (velos only) — checks
 * the *saved* config, never mutates settings. Callers manage their own
 * per-vendor pending/result display since multiple checks can run at once
 * (e.g. one per vendor right after a save).
 */
export function useTestVendor() {
  return useMutation({
    mutationFn: (name: string) => api.config.testVendor(name),
  });
}
