import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type {
  MemoryCreateInput,
  MemorySpaceCreateInput,
  MemorySpaceUpdateInput,
  MemoryUpdateInput,
} from "../api/types";

export const memorySpacesKey = ["memory-spaces"] as const;
export const memoriesKey = (space?: string) =>
  ["memories", space ?? "all"] as const;

/** All memory spaces, each with its memory count. */
export function useMemorySpaces() {
  return useQuery({
    queryKey: memorySpacesKey,
    queryFn: () => api.memory.listSpaces(),
  });
}

/** Memories in one space; pass undefined for every space. */
export function useMemories(space?: string) {
  return useQuery({
    queryKey: memoriesKey(space),
    queryFn: () => api.memory.list(space),
  });
}

/** Invalidate both lists — a memory write changes a space's count too. */
function useRefresh() {
  const client = useQueryClient();
  return () => {
    void client.invalidateQueries({ queryKey: memorySpacesKey });
    void client.invalidateQueries({ queryKey: ["memories"] });
  };
}

export function useCreateSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (body: MemorySpaceCreateInput) => api.memory.createSpace(body),
    onSuccess: refresh,
  });
}

export function useUpdateSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: ({
      name,
      body,
    }: {
      name: string;
      body: MemorySpaceUpdateInput;
    }) => api.memory.updateSpace(name, body),
    onSuccess: refresh,
  });
}

export function useDeleteSpace() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (name: string) => api.memory.deleteSpace(name),
    onSuccess: refresh,
  });
}

export function useCreateMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (body: MemoryCreateInput) => api.memory.create(body),
    onSuccess: refresh,
  });
}

export function useUpdateMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: ({ id, body }: { id: number; body: MemoryUpdateInput }) =>
      api.memory.update(id, body),
    onSuccess: refresh,
  });
}

export function useDeleteMemory() {
  const refresh = useRefresh();
  return useMutation({
    mutationFn: (id: number) => api.memory.remove(id),
    onSuccess: refresh,
  });
}
