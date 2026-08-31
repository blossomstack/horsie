import { Trash2 } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

/**
 * One entry in a roster: agents, environments, routines, workflows.
 *
 * These four pages render the same thing — a named item, a description, and a
 * way to delete it — and each had drifted into its own dialect. The names were
 * mono on two pages and the reading face on the other two; the delete control
 * was a `key-icon` here, a `key-danger` key there, and a hand-rolled
 * `rounded-chip p-1.5` on the third, at three different icon sizes. None of
 * that was a decision anybody made — it is what four pages written on four
 * different days look like.
 *
 * So the row is a component, not a pattern. A fifth roster gets it for free,
 * and a change to the language happens once.
 *
 * What a row may say is deliberately short: a name, a description, and at most
 * one line of fact. The counts and stamps that used to hang off every row
 * ("3 skills", "2 memory", "ran 2h ago") made a 20rem column of five-line
 * entries you had to read rather than scan — and all of it is in the panel
 * beside the roster, one click away.
 */
export function RosterRow({
  to,
  name,
  meta,
  description,
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
  /** The one fact worth scanning for, if there is one — a routine's schedule.
   * Not "what it is set to": that belongs in the panel. */
  meta?: ReactNode;
  description?: string;
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
            column now, and anything beside the name was eating it: a workflow
            called `nightly-release` read as `nightly…` next to "2 steps ·
            starts at build". A name is what you scan a roster for. */}
        <span className="item-title block truncate">{name}</span>
        {description && (
          <span className="mt-0.5 block truncate text-xs text-dim">
            {description}
          </span>
        )}
        {meta && <span className="legend mt-0.5 block truncate">{meta}</span>}
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
