import { ChevronsDown, ChevronsUp } from "lucide-react";
import type { RenderedCompaction } from "../hooks/useSessionStream";

/** Seeking across a session's sessions.
 *
 * A thin column down the edge of the transcript: a cap at each end for the very
 * start and the very end, and one tick per compaction boundary in between,
 * placed in proportion so a long session's shape is legible at a glance.
 *
 * Named a spine, not a rail, because `rail.tsx` is already the session list —
 * two different things called the same thing is how a codebase gets confusing.
 *
 * With no compactions it is just the two caps. It does not appear and disappear
 * as a session's shape changes: a control that comes and goes is one you have
 * to notice before you can use it, and jump-to-start is useful in a long
 * session whether or not it has ever compacted.
 */
export function TranscriptSpine({
  boundaries,
  onSeek,
}: {
  /** Every compaction boundary, oldest first. */
  boundaries: RenderedCompaction[];
  /** Scroll to a boundary by its seq, or to one end of the transcript. */
  onSeek: (target: number | "start" | "end") => void;
}) {
  return (
    <div
      data-testid="transcript-spine"
      className="pointer-events-none absolute inset-y-0 right-0 z-10 hidden w-8 select-none lg:block"
      aria-hidden={false}
    >
      <div className="pointer-events-auto sticky top-0 flex h-full flex-col items-center py-6">
        <Cap
          label="Jump to the start of the session"
          testid="spine-start"
          onClick={() => onSeek("start")}
        >
          <ChevronsUp size={12} aria-hidden />
        </Cap>

        <div className="relative my-2 w-px flex-1 bg-[var(--rule)]">
          {boundaries.map((b, i) => (
            <button
              key={b.seq}
              data-testid="spine-tick"
              data-seq={b.seq}
              onClick={() => onSeek(b.seq)}
              // Evenly spaced rather than positioned by scroll offset: the
              // offsets are not known until everything above has rendered and
              // measured, and a tick that jumps around as images and code
              // blocks settle is worse than one that never moves.
              style={{ top: `${((i + 1) / (boundaries.length + 1)) * 100}%` }}
              className="absolute left-1/2 h-2.5 w-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full border border-[var(--rule-strong)] bg-[var(--surface)] transition-colors hover:border-amber hover:bg-amber"
              title={
                b.covered === null
                  ? `Session ${i + 1} ended here`
                  : `Session ${i + 1} ended here — ${b.covered} entries summarised`
              }
              aria-label={`Jump to compaction ${i + 1} of ${boundaries.length}`}
            />
          ))}
        </div>

        <Cap
          label="Jump to the end of the session"
          testid="spine-end"
          onClick={() => onSeek("end")}
        >
          <ChevronsDown size={12} aria-hidden />
        </Cap>
      </div>
    </div>
  );
}

function Cap({
  label,
  testid,
  onClick,
  children,
}: {
  label: string;
  testid: string;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      data-testid={testid}
      onClick={onClick}
      title={label}
      aria-label={label}
      className="flex h-5 w-5 items-center justify-center rounded-full text-faint transition-colors hover:text-amber-ink"
    >
      {children}
    </button>
  );
}
