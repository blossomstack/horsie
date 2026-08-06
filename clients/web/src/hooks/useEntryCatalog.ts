import { useMemo } from "react";
import type { CatalogEntryView } from "../api/types";
import { usePlugins } from "./usePlugins";

/**
 * Everything the selected bundles offer, for the composer's typeahead.
 *
 * Reads the bundle list the settings page and the new-session picker already
 * fetch — no session and no runtime, which is what lets completions work on the
 * new-session screen and change as bundles are checked and unchecked.
 *
 * `selected` empty resolves to the default-enabled bundles, mirroring what the
 * server does with the same input at provisioning: a session that chose nothing
 * still gets the defaults, so the menu must show them.
 *
 * A name two bundles both declare goes to the first, matching the server's
 * rule — otherwise the menu would offer an entry that expands to something
 * else.
 */
export function useEntryCatalog(
  selected: Iterable<string> | undefined,
): CatalogEntryView[] {
  const { data: bundles } = usePlugins();
  // The draft hands out a fresh `Set` every render, so the memo keys on the
  // names themselves rather than the container's identity.
  const names = selected ? [...selected] : [];
  // NUL-joined: a separator no bundle name can contain, and `key` is only ever
  // compared for equality.
  const key = [...names].sort().join("\u0000");
  return useMemo(() => {
    if (!bundles) return [];
    const active =
      names.length > 0
        ? bundles.filter((b) => names.includes(b.name))
        : bundles.filter((b) => b.enabledDefault);
    const seen = new Set<string>();
    const out: CatalogEntryView[] = [];
    for (const bundle of [...active].sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      for (const entry of bundle.catalog ?? []) {
        // Keyed by name *and* kind-group: `/review` and `@review` are two
        // different things to type, so they do not collide.
        const handle = `${entry.kind === "agent" ? "@" : "/"}${entry.name}`;
        if (seen.has(handle)) continue;
        seen.add(handle);
        out.push(entry);
      }
    }
    return out;
  }, [bundles, key]);
}
