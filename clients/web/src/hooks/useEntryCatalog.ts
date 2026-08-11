import { useMemo } from "react";
import type { CatalogEntryView } from "../api/types";
import { useBuiltins, usePlugins } from "./usePlugins";

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
 *
 * Built-ins lead the list for the same reason the server consults them first:
 * a bundle that declares `/compact` must not be able to take over a control the
 * product owns, and the menu has to agree with what actually runs.
 */
export function useEntryCatalog(
  selected: Iterable<string> | undefined,
): CatalogEntryView[] {
  const { data: bundles } = usePlugins();
  const { data: builtins } = useBuiltins();
  // The draft hands out a fresh `Set` every render, so the memo keys on the
  // names themselves rather than the container's identity.
  const names = selected ? [...selected] : [];
  // NUL-joined: a separator no bundle name can contain, and `key` is only ever
  // compared for equality.
  const key = [...names].sort().join("\u0000");
  return useMemo(() => {
    // Built-ins do not wait on the bundle list: they are offered in a session
    // that has none, which is the case the plugin catalogue cannot express.
    const seeded = builtins ?? [];
    if (!bundles) return seeded;
    const active =
      names.length > 0
        ? bundles.filter((b) => names.includes(b.name))
        : bundles.filter((b) => b.enabledDefault);
    const seen = new Set<string>(seeded.map((e) => `/${e.name}`));
    const out: CatalogEntryView[] = [...seeded];
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
  }, [bundles, builtins, key]);
}
