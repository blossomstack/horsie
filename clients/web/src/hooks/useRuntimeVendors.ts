import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { RuntimeVendorConfigInput } from "../api/types";

export const runtimeVendorKeys = {
  all: ["runtime-vendors"] as const,
};

/** Runtime vendors configured on this server. */
export function useRuntimeVendors() {
  return useQuery({
    queryKey: runtimeVendorKeys.all,
    queryFn: () => api.runtimeVendors.list(),
  });
}

export function useSaveRuntimeVendor() {
  const qc = useQueryClient();
  return useMutation({
    // The form renders this one directly above its Save button.
    meta: { inlineError: true },
    mutationFn: ({
      name,
      body,
    }: {
      name: string;
      body: RuntimeVendorConfigInput;
    }) => api.runtimeVendors.save(name, body),
    // Also the settings view: a saved vendor is selectable immediately, and
    // the roster there is what a user looks at to confirm that.
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: runtimeVendorKeys.all });
      qc.invalidateQueries({ queryKey: ["settings"] });
    },
  });
}

/**
 * Ask a saved vendor's substrate whether it still answers.
 *
 * No cache to invalidate: the outcome is not stored anywhere, because a claim
 * about a remote credential stops being true the moment someone revokes it —
 * so the only honest place for it is the row that asked, until the page is left.
 */
export function useTestRuntimeVendor() {
  return useMutation({
    mutationFn: (name: string) => api.runtimeVendors.test(name),
  });
}

export function useDeleteRuntimeVendor() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.runtimeVendors.remove(name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: runtimeVendorKeys.all });
      qc.invalidateQueries({ queryKey: ["settings"] });
    },
  });
}
