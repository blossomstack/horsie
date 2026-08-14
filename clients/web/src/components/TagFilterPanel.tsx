import { Check, Minus } from "lucide-react";
import { cn } from "../lib/cn";
import {
  cycleTag,
  filterIsActive,
  tagState,
  type TagFilter,
} from "../lib/sessionTags";

/** The tag chips, between the Sessions title and the list. Three states per
 * chip, because "show me web" and "hide anything done" are both filters and a
 * checkbox can only say the first. */
export function TagFilterPanel({
  tags,
  filter,
  onChange,
}: {
  tags: string[];
  filter: TagFilter;
  onChange: (next: TagFilter) => void;
}) {
  return (
    <div
      className="flex flex-wrap items-center gap-1 px-2 pb-2"
      data-testid="tag-filter-panel"
    >
      {tags.map((t) => {
        const state = tagState(filter, t);
        return (
          <button
            key={t}
            type="button"
            data-testid={`tag-chip-${t}`}
            // `aria-pressed` has two values and this control has three, so the
            // state rides the accessible name instead.
            data-state={state}
            aria-label={
              state === "require"
                ? `${t} — required`
                : state === "exclude"
                  ? `${t} — excluded`
                  : t
            }
            className={cn(
              "chip transition-colors hover:!text-legend",
              state === "require" &&
                "!border-[var(--rule-strong)] !bg-raised !text-legend",
              state === "exclude" && "!text-faint line-through",
            )}
            onClick={() => onChange(cycleTag(filter, t))}
          >
            {state === "require" && <Check size={10} aria-hidden />}
            {state === "exclude" && <Minus size={10} aria-hidden />}
            {t}
          </button>
        );
      })}
      {filterIsActive(filter) && (
        <button
          type="button"
          data-testid="clear-tag-filter"
          className="legend px-1.5 py-0.5 hover:!text-legend"
          onClick={() => onChange({ require: [], exclude: [] })}
        >
          Clear
        </button>
      )}
    </div>
  );
}
