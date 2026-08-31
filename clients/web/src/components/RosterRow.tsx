import { Trash2 } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

/**
 * One entry in a roster: agents, environments, routines, workflows.
 *
 * These four pages render the same thing — a named item, what it is set to,
 * a description, some counts, and a way to delete it — and each had drifted
 * into its own dialect. The names were mono on two pages and the reading face
 * on the other two; the delete control was a `key-icon` here, a `key-danger`
 * key there, and a hand-rolled `rounded-chip p-1.5` on the third, at three
 * different icon sizes; the identity line was baseline-aligned on two and
 * centred on the rest. None of that was a decision anybody made — it is what
 * four pages written on four different days look like.
 *
 * So the row is a component, not a pattern. A fifth roster gets it for free,
 * and a change to the language happens once.
 */
export function RosterRow({
  to,
  name,
  meta,
  description,
  facts,
  aside,
  onDelete,
  deleteLabel,
  deleteTestId,
  testId,
  nameAttr,
  selected,
}: {
  /** Where the name goes. */
  to: string;
  name: string;
  /** What it is set to — the model, the vendor, the schedule. On its own line
   * under the name, in the label voice, because it qualifies the name rather
   * than describing the thing. */
  meta?: ReactNode;
  description?: string;
  /** Counts and stamps under the description: "3 skills", "ran 2h ago". */
  facts?: ReactNode;
  /** Joined onto the end of the meta line — a relative time, usually. */
  aside?: ReactNode;
  onDelete: () => void;
  deleteLabel: string;
  deleteTestId: string;
  testId: string;
  /** `[data-agent-name]`, `[data-routine-name]`, … — the e2e suites select on
   * these, so each caller keeps naming its own. */
  nameAttr?: Record<string, string>;
  /** This is the item the panel beside the roster is showing. */
  selected?: boolean;
}) {
  return (
    <div
      className="row px-2.5 py-2"
      data-testid={testId}
      aria-selected={selected}
      {...nameAttr}
    >
      <Link to={to} className="min-w-0 flex-1">
        {/* The name gets the line to itself. These rosters live in a 20rem
            column now, and `meta` beside the name was eating it: a workflow
            called `nightly-release` read as `nightly…` next to "2 steps ·
            starts at build". A name is what you scan a roster for. */}
        <span className="item-title block truncate">{name}</span>
        {(meta || aside) && (
          <span className="legend mt-0.5 block truncate">
            {meta}
            {meta && aside ? " · " : ""}
            {aside}
          </span>
        )}
        {description && (
          <span className="mt-0.5 block truncate text-xs text-dim">
            {description}
          </span>
        )}
        {facts && (
          <span className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5">{facts}</span>
        )}
      </Link>
      <button
        className="key-icon shrink-0 hover:!bg-red-quiet hover:!text-red-ink"
        title={deleteLabel}
        aria-label={deleteLabel}
        data-testid={deleteTestId}
        onClick={onDelete}
      >
        <Trash2 size={15} />
      </button>
    </div>
  );
}
