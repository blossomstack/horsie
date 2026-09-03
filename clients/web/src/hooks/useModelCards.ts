import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { ModelCardInput, ModelCardUpdate } from "../api/types";

export const modelCardsKey = ["model-cards"] as const;

/** Public prefix search — backs the Settings model-id autocomplete. */
export function useModelCardSearch(prefix: string, enabled = true) {
  return useQuery({
    queryKey: [...modelCardsKey, "search", prefix],
    queryFn: () => api.modelCards.search(prefix),
    enabled,
  });
}

/** The full catalog, for the Settings model-cards page. */
export function useModelCards() {
  return useQuery({
    queryKey: [...modelCardsKey, "list"],
    queryFn: () => api.modelCards.list(),
  });
}

export function useCreateModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (body: ModelCardInput) => api.modelCards.create(body),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}

export function useUpdateModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({
      modelId,
      body,
    }: {
      modelId: string;
      body: ModelCardUpdate;
    }) => api.modelCards.update(modelId, body),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}

export function useDeleteModelCard() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (modelId: string) => api.modelCards.remove(modelId),
    onSuccess: () => client.invalidateQueries({ queryKey: modelCardsKey }),
  });
}
