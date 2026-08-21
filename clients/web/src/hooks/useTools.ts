import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { ToolCatalog, ToolGroupView, ToolView } from "../api/types";

export const toolsKey = ["tools"] as const;

/**
 * The built-in tools this server offers, grouped.
 *
 * A table compiled into the server, so it cannot change while the app is open —
 * `staleTime: Infinity`, like `useBuiltins`. Nothing here is fetched per
 * session or per vendor.
 */
export function useTools() {
  return useQuery({
    queryKey: toolsKey,
    queryFn: () => api.tools.catalog(),
    staleTime: Infinity,
  });
}

/** Every tool in the catalogue, flattened. */
export function allTools(catalog: ToolCatalog | undefined): ToolView[] {
  return (catalog?.groups ?? []).flatMap((g: ToolGroupView) => g.tools);
}

/**
 * What an absent selection resolves to. Mirrors `crate::tools::default_set` —
 * the server is the authority, and says so per tool with `inDefaultSet`, so
 * this stays a filter rather than a second copy of the rule.
 */
export function defaultSelection(catalog: ToolCatalog | undefined): Set<string> {
  return new Set(
    allTools(catalog)
      .filter((t) => t.inDefaultSet)
      .map((t) => t.name),
  );
}

/**
 * Drop names the server no longer offers.
 *
 * A selection is persisted (a draft in localStorage, a preset in the database)
 * and the catalogue is not, so a horsie that renamed or removed a tool leaves
 * dead names behind. Returns the same reference when nothing changed, so a
 * caller can use it as a "did anything change" test without a deep compare.
 */
export function filterKnownTools(
  selected: Set<string>,
  catalog: ToolCatalog | undefined,
): Set<string> {
  if (!catalog) return selected;
  const known = new Set(allTools(catalog).map((t) => t.name));
  const kept = [...selected].filter((n) => known.has(n));
  return kept.length === selected.size ? selected : new Set(kept);
}
