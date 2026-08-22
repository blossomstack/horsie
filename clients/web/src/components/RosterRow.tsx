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
}: {
  /** Where the name goes. */
  to: string;
  name: string;
  /** What it is set to — the model, the vendor, the schedule. Sits beside the
   * name, in the label voice, because it qualifies the name rather than
   * describing the thing. */
  meta?: ReactNode;
  description?: string;
  /** Counts and stamps under the description: "3 skills", "ran 2h ago". */
  facts?: ReactNode;
  /** Right-aligned, before the actions — a relative time, usually. */
  aside?: ReactNode;
  onDelete: () => void;
  deleteLabel: string;
  deleteTestId: string;
  testId: string;
  /** `[data-agent-name]`, `[data-routine-name]`, … — the e2e suites select on
   * these, so each caller keeps naming its own. */
  nameAttr?: Record<string, string>;
}) {
  return (
    <div className="row px-2.5 py-2" data-testid={testId} {...nameAttr}>
      <Link to={to} className="min-w-0 flex-1">
        <span className="flex items-baseline gap-2">
          <span className="item-title truncate">{name}</span>
          {meta && <span className="legend shrink-0">{meta}</span>}
        </span>
        {description && (
          <span className="mt-0.5 block truncate text-xs text-dim">
            {description}
          </span>
        )}
        {facts && (
          <span className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5">{facts}</span>
        )}
      </Link>
      {aside && <span className="shrink-0 text-xs text-faint">{aside}</span>}
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
