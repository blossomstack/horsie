import type { SessionSummary } from "../api/types";

/** A tag lives in its own annotation namespace, so a future `source=` or
 * `origin=` key can never be mistaken for one. */
export const TAG_PREFIX = "tag.";

/** The annotation key charset the server enforces is 128 characters; the
 * prefix spends four of them. */
const MAX_TAG_LEN = 124;

export interface TagFilter {
  require: string[];
  exclude: string[];
}

export const EMPTY_FILTER: TagFilter = { require: [], exclude: [] };

/** This session's tags, sorted. `tag.` with nothing after it is not a tag. */
export function sessionTags(s: SessionSummary): string[] {
  return s.annotations
    .filter(
      (a) => a.key.startsWith(TAG_PREFIX) && a.key.length > TAG_PREFIX.length,
    )
    .map((a) => a.key.slice(TAG_PREFIX.length))
    .sort();
}

/** Every tag in existence. Derived, never stored: this is what makes a tag
 * appear the moment it is first used and vanish when its last carrier drops
 * it, with nothing to register and nothing to garbage-collect. */
export function allTags(sessions: SessionSummary[]): string[] {
  const seen = new Set<string>();
  for (const s of sessions) for (const t of sessionTags(s)) seen.add(t);
  return [...seen].sort();
}

/** What the user typed, as a tag the server will accept — or `undefined` when
 * nothing survives normalising. Rejecting `Bug Fix` outright would be
 * pedantry; `bug-fix` is what they meant. */
export function normalizeTagName(raw: string): string | undefined {
  const name = raw
    .trim()
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9._-]/g, "");
  if (!name || name.length > MAX_TAG_LEN) return undefined;
  return name;
}

export function filterIsActive(f: TagFilter): boolean {
  return f.require.length > 0 || f.exclude.length > 0;
}

export function matchesTagFilter(s: SessionSummary, f: TagFilter): boolean {
  if (!filterIsActive(f)) return true;
  const tags = new Set(sessionTags(s));
  return (
    f.require.every((t) => tags.has(t)) && !f.exclude.some((t) => tags.has(t))
  );
}

export function tagState(
  f: TagFilter,
  tag: string,
): "neutral" | "require" | "exclude" {
  if (f.require.includes(tag)) return "require";
  if (f.exclude.includes(tag)) return "exclude";
  return "neutral";
}

/** neutral → require → exclude → neutral. Three states because "show me web"
 * and "hide anything done" are both filters, and a checkbox can only say the
 * first. */
export function cycleTag(f: TagFilter, tag: string): TagFilter {
  switch (tagState(f, tag)) {
    case "neutral":
      return { require: [...f.require, tag], exclude: f.exclude };
    case "require":
      return {
        require: f.require.filter((t) => t !== tag),
        exclude: [...f.exclude, tag],
      };
    case "exclude":
      return {
        require: f.require,
        exclude: f.exclude.filter((t) => t !== tag),
      };
  }
}

/** Drop constraints for tags that no longer exist. A persisted filter naming
 * a tag whose last session was deleted would hide the whole rail with no
 * visible cause — the chip that explains it is not even rendered. */
export function reconcileFilter(
  saved: TagFilter,
  universe: string[],
): TagFilter {
  const live = new Set(universe);
  return {
    require: saved.require.filter((t) => live.has(t)),
    exclude: saved.exclude.filter((t) => live.has(t)),
  };
}
