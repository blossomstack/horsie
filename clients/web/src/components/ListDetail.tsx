import type { ReactNode } from "react";
import { RailToggle } from "./rail";
import { useScrolledUnder } from "../hooks/useScrolledUnder";

/**
 * A roster beside what is selected in it.
 *
 * The inbox had this shape and the four resource pages did not: they were
 * full-width lists that navigated *away* to read one item, so choosing between
 * two agents meant going back and forth through a list that scrolled itself to
 * the top each time. Reading one thing and seeing the others is the whole point
 * of a roster, and one page already knew it.
 *
 * The shell only, deliberately. What the rows look like is `RosterRow`'s
 * business and what the panel holds is each page's, so this owns exactly the
 * two things every one of them was writing out by hand and getting slightly
 * differently: the column widths and the header height.
 */
export function ListDetail({
  title,
  action,
  filters,
  children,
  detail,
  testId,
}: {
  title: string;
  /** The one control that creates a new item, beside the title. */
  action?: ReactNode;
  /** An optional row under the header — the inbox's read/unread chips. */
  filters?: ReactNode;
  /** The roster itself. */
  children: ReactNode;
  /** What is selected, or a line saying nothing is. */
  detail: ReactNode;
  testId: string;
}) {
  const { onScroll, barProps } = useScrolledUnder();
  return (
    <div className="flex h-full" data-testid={testId}>
      <div className="column-edge-r flex h-full w-[20rem] shrink-0 flex-col bg-panel">
        <header
          {...barProps}
          className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 px-4"
        >
          <RailToggle />
          <h1 className="page-title min-w-0 flex-1 truncate">{title}</h1>
          {action}
        </header>
        {filters}
        <div className="flex-1 overflow-y-auto px-2 pb-2" onScroll={onScroll}>
          {children}
        </div>
      </div>
      <div className="min-w-0 flex-1 overflow-y-auto">{detail}</div>
    </div>
  );
}

/**
 * The right-hand column with nothing chosen.
 *
 * Its own component so the four rosters cannot drift into four ways of saying
 * the same nothing — which is exactly what happened to the rows before
 * `RosterRow`.
 */
export function NothingSelected({ children }: { children: ReactNode }) {
  return (
    <p className="empty" data-testid="nothing-selected">
      {children}
    </p>
  );
}
