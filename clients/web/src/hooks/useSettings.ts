import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback } from "react";
import { api } from "../api/client";
import type {
  ModelInput,
  ModelView,
  ProviderInput,
  ProviderView,
  SettingsView,
} from "../api/types";

export const settingsKey = ["settings"] as const;

/** The server's runtime-editable configuration (providers, models, vendors). */
export function useSettings() {
  return useQuery({ queryKey: settingsKey, queryFn: () => api.config.get() });
}

/**
 * Refetch the settings, for a change made through some other endpoint.
 *
 * A ChatGPT sign-in is the case that needs it: it writes a credential through
 * `/api/config/model-providers/:name/chatgpt`, which the settings mutation knows
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

/**
 * Per-resource settings mutations.
 *
 * Each one touches a single row, so two in flight cannot discard each other's
 * edit the way the old whole-document save could. They invalidate the settings
 * query rather than seeding it, which is the cheapest way to stay consistent
 * when several run at once.
 */
function useSettingsMutation<TArgs, TResult>(
  fn: (args: TArgs) => Promise<TResult>,
) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: () => void client.invalidateQueries({ queryKey: settingsKey }),
  });
}

/** Create or replace one model alias. */
export function usePutModel() {
  return useSettingsMutation<{ alias: string; body: ModelInput }, ModelView>(
    ({ alias, body }) => api.config.models.put(alias, body),
  );
}

/** Remove one model alias. */
export function useDeleteModel() {
  return useSettingsMutation<string, void>((alias) =>
    api.config.models.remove(alias),
  );
}

/** Create or replace one provider. Omitting `apiKey` keeps the stored key. */
export function usePutProvider() {
  return useSettingsMutation<
    { name: string; body: ProviderInput },
    ProviderView
  >(({ name, body }) => api.config.modelProviders.put(name, body));
}

/** Remove one provider. Rejected with 409 while a model still routes to it. */
export function useDeleteProvider() {
  return useSettingsMutation<string, void>((name) =>
    api.config.modelProviders.remove(name),
  );
}

/**
 * Set or clear the vendor new sessions default to; seeds the cache with the
 * new view.
 *
 * Passing `null` clears the preference. That is a separate endpoint rather than
 * an empty string, because an empty vendor name is refused — and because the
 * old whole-document save expressed "clear" as an omitted field, which the
 * server read as "leave unchanged", so the button silently did nothing.
 */
export function useSetDefaultRuntimeVendor() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (vendor: string | null) =>
      vendor === null
        ? api.config.clearDefaultRuntimeVendor()
        : api.config.setDefaultRuntimeVendor(vendor),
    onSuccess: (view: SettingsView) => client.setQueryData(settingsKey, view),
  });
}

/**
 * On-demand connection check for a configured vendor (velos only) — checks
 * the *saved* config, never mutates settings. Callers manage their own
 * per-vendor pending/result display since multiple checks can run at once
 * (e.g. one per vendor right after a save).
 */
