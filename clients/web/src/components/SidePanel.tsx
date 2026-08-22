import { ChevronsRight } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "../lib/cn";

/**
 * The third column: a panel beside the session's body.
 *
 * One component because there are now three of them — the plan, one agent's
 * numbers, one transcript entry — and they are one thing: a column that opens
 * on the right, overlays below `lg`, and closes. They had started to drift
 * apart at the edges (the scrim, the header height, what the close key looks
 * like), which is the point at which a reader stops reading them as the same
 * surface.
 *
 * The header carries a legend, an optional readout beside it, and the close
 * key. Everything else is the caller's.
 */
export function SidePanel({
  legend,
  readout,
  onClose,
  closeLabel,
  testId,
  closeTestId,
  children,
}: {
  /** What this panel is, in the legend voice: "Plan", "Agent", "Entry". */
  legend: string;
  /** A short figure beside the legend — a count, a fraction, a kind. Supplied
   *  as a node rather than a string so a caller keeps its own testid: these
   *  panels are addressed by name from the e2e suite, and a shared shell is
   *  not a reason to rename what a caller was already called. */
  readout?: ReactNode;
  onClose: () => void;
  /** For the scrim and the close key, which both need to name what closes. */
  closeLabel: string;
  testId: string;
  /** The close key's own testid, for the same reason. */
  closeTestId: string;
  children: ReactNode;
}) {
  return (
    <>
      {/* Below lg the panel is an overlay, so it needs a scrim. Without one the
          body behind stayed scrollable and tappable, and on a phone the panel
          covered the session header — including the very key that opens it —
          leaving the chevron as the only way back out. */}
      <button
        type="button"
        className="fixed inset-0 z-10 cursor-default bg-chassis/60 lg:hidden"
        onClick={onClose}
        aria-label={closeLabel}
        data-testid={`${testId}-scrim`}
      />
      <aside
        // A third column below lg leaves nothing to read, so it overlays. Below
        // sm a 16rem overlay left a sliver of body with code clipped mid-token,
        // so there it takes the full width instead of pretending to still be a
        // column.
        className={cn(
          "column-edge-l flex w-72 shrink-0 flex-col bg-panel",
          "max-lg:absolute max-lg:inset-y-0 max-lg:right-0 max-lg:z-20",
          "max-lg:shadow-[var(--float)] max-sm:w-full",
        )}
        data-testid={testId}
      >
        <div className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 px-3">
          <h2 className="legend !text-dim">{legend}</h2>
          {readout}
          <button
            className="key-icon ml-auto !h-7 !w-7"
            onClick={onClose}
            title={closeLabel}
            aria-label={closeLabel}
            data-testid={closeTestId}
          >
            <ChevronsRight size={14} aria-hidden />
          </button>
        </div>
        {children}
      </aside>
    </>
  );
}
