import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback } from "react";
import { api } from "../api/client";
import type { SettingsUpdate, SettingsView } from "../api/types";

export const settingsKey = ["settings"] as const;

/** The server's runtime-editable configuration (providers, models, vendors). */
export function useSettings() {
  return useQuery({ queryKey: settingsKey, queryFn: () => api.config.get() });
}

/**
 * Refetch the settings, for a change made through some other endpoint.
 *
 * A ChatGPT sign-in is the case that needs it: it writes a credential through
 * `/api/admin/providers/:name/chatgpt`, which the settings mutation knows
 * nothing about, yet it moves `hasCredential` — and with it the provider lamp
 * and whether models can be added.
 */
export function useRefreshSettings() {
  const client = useQueryClient();
  return useCallback(
    () => void client.invalidateQueries({ queryKey: settingsKey }),
    [client],
  );
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
