import type { CatalogEntryView } from "../api/types";
import { cn } from "../lib/cn";

/**
 * The `/` and `@` typeahead, filtered to what the user has typed so far.
 *
 * Presentational: it owns no state and decides nothing about when it is shown.
 * The composer knows whether the field currently holds an invocation, and this
 * knows how to show one — keeping the two apart is what stops the composer,
 * which is deliberately 142 lines, from growing a menu inside it.
 */
export function EntryMenu({
  entries,
  activeIndex,
  onPick,
}: {
  entries: CatalogEntryView[];
  activeIndex: number;
  onPick: (entry: CatalogEntryView) => void;
}) {
  if (entries.length === 0) return null;
  return (
    <ul
      className="panel absolute bottom-full left-0 right-0 z-10 mb-2 max-h-64 overflow-y-auto py-1"
      role="listbox"
      aria-label="Commands, skills and agents"
      data-testid="entry-menu"
    >
      {entries.map((entry, i) => (
        <li key={`${entry.kind}:${entry.name}`}>
          {/* `onMouseDown` rather than `onClick`: a click would blur the
              textarea first, and a blur that closes the menu eats the pick. */}
          <button
            type="button"
            role="option"
            aria-selected={i === activeIndex}
            onMouseDown={(e) => {
              e.preventDefault();
              onPick(entry);
            }}
            className={cn(
              "flex w-full items-baseline gap-2 px-3 py-1.5 text-left text-[0.8125rem]",
              i === activeIndex ? "bg-raised text-legend" : "text-dim",
            )}
          >
            <code className="shrink-0">
              {entry.kind === "agent" ? "@" : "/"}
              {entry.name}
            </code>
            {entry.argumentHint && (
              <span className="shrink-0 text-faint">{entry.argumentHint}</span>
            )}
            <span className="truncate text-faint">{entry.description}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

/**
 * The invocation the field currently holds, if any.
 *
 * Leading position only, and only while the name is still being typed — the
 * same rule the server's parser uses, so the menu never offers a completion for
 * something the server would send verbatim. Once there is whitespace after the
 * name the user has moved on to arguments and the menu gets out of the way.
 */
export function invocationPrefix(
  text: string,
): { sigil: string; query: string } | null {
  // `\p{L}\p{N}` rather than `\w`, which is ASCII-only: the server accepts any
  // alphanumeric in a name, and a menu that cannot offer a name the server
  // would accept is a menu that lies about it.
  const m = /^([/@])([\p{L}\p{N}_-]*)$/u.exec(text);
  return m ? { sigil: m[1], query: m[2] } : null;
}

/** Entries reachable by `sigil` whose name or description matches `query`. */
export function filterEntries(
  entries: CatalogEntryView[],
  sigil: string,
  query: string,
): CatalogEntryView[] {
  const q = query.toLowerCase();
  return entries.filter((e) => {
    if ((e.kind === "agent" ? "@" : "/") !== sigil) return false;
    if (q === "") return true;
    return (
      e.name.toLowerCase().includes(q) ||
      e.description.toLowerCase().includes(q)
    );
  });
}
