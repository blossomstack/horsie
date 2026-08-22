import { ChevronsDown, ChevronsUp } from "lucide-react";
import { useRef } from "react";
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
  view,
  progress,
  onScrollTo,
}: {
  /** Every compaction boundary, oldest first. */
  boundaries: RenderedCompaction[];
  /** Scroll to a boundary by its seq, or to one end of the transcript. */
  onSeek: (target: number | "start" | "end") => void;
  /** Visible fraction of the transcript, 0-1. At 1 there is nothing to scroll
   * and no thumb is drawn. */
  view: number;
  /** How far down the scroll is, 0-1. */
  progress: number;
  /** Scroll to a fraction of the way down. */
  onScrollTo: (fraction: number) => void;
}) {
  const trackRef = useRef<HTMLDivElement>(null);
  // A very long session would otherwise leave nothing to grab.
  const thumbFraction = Math.max(view, 0.06);
  const scrollable = view < 0.999;

  const driveAt = (clientY: number, grabOffset: number) => {
    const track = trackRef.current;
    if (!track) return;
    const rect = track.getBoundingClientRect();
    const span = rect.height - thumbFraction * rect.height;
    if (span <= 0) return;
    onScrollTo(Math.min(1, Math.max(0, (clientY - rect.top - grabOffset) / span)));
  };

  /* The drag runs on WINDOW listeners.
   *
   * `setPointerCapture` on the track looked right and did nothing: the
   * pointerdown lands on the thumb, so the track never becomes the capture
   * target. The window hears these wherever the pointer wanders, which for a
   * 3px-wide control is immediately. */
  const startDrag = (grabOffset: number) => {
    const move = (ev: PointerEvent) => driveAt(ev.clientY, grabOffset);
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  return (
    <div
      data-testid="transcript-spine"
      className="pointer-events-none absolute inset-y-0 right-0 z-10 hidden w-8 select-none lg:block"
      aria-hidden={false}
    >
      {/* No `sticky`. This used to live INSIDE the scroller, where
       * `inset-y-0` made its box the whole scroll height and `sticky` dragged
       * it back into view on every frame — so it drifted with the content and
       * its real coordinates were never where it appeared to be. It is now a
       * sibling of the scroller, pinned to the pane, and simply does not
       * move. */}
      <div className="pointer-events-auto flex h-full flex-col items-center py-6">
        <Cap
          label="Jump to the start of the session"
          testid="spine-start"
          onClick={() => onSeek("start")}
        >
          <ChevronsUp size={12} aria-hidden />
        </Cap>

        {/* The hit area is the column; the visible track is the hairline
         * inside it. A 1px pointer target is one nobody can hit, and a
         * scrollbar you have to aim at is not a scrollbar. */}
        <div
          ref={trackRef}
          className="relative my-2 w-5 flex-1 cursor-pointer"
          onPointerDown={(e) => {
            if ((e.target as HTMLElement).dataset.spineThumb !== undefined) return;
            const rect = e.currentTarget.getBoundingClientRect();
            const grab = (thumbFraction * rect.height) / 2;
            driveAt(e.clientY, grab);
            startDrag(grab);
          }}
        >
          <span
            aria-hidden
            className="pointer-events-none absolute inset-y-0 left-1/2 w-px -translate-x-1/2 bg-[var(--rule)]"
          />
          {/* The transcript's scrollbar. The native one is hidden at this
           * width because this says the same thing in the transcript's own
           * vocabulary — on the line the compaction ticks already sit on,
           * rather than a second bar a few pixels to its right saying it
           * again. */}
          {scrollable && (
            <div
              data-spine-thumb=""
              data-testid="spine-thumb"
              role="scrollbar"
              aria-orientation="vertical"
              aria-label="Scroll the transcript"
              aria-valuenow={Math.round(progress * 100)}
              aria-valuemin={0}
              aria-valuemax={100}
              className="absolute left-1/2 w-[3px] -translate-x-1/2 rounded-full bg-[var(--rule-strong)] transition-colors hover:bg-[var(--legend-faint)]"
              style={{
                height: `${thumbFraction * 100}%`,
                top: `${progress * (100 - thumbFraction * 100)}%`,
              }}
              onPointerDown={(e) => {
                e.preventDefault();
                startDrag(e.clientY - e.currentTarget.getBoundingClientRect().top);
              }}
            />
          )}
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
              className="absolute left-1/2 h-2.5 w-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full border border-[var(--rule-strong)] bg-[var(--panel)] transition-colors hover:border-live hover:bg-live"
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
      className="flex h-5 w-5 items-center justify-center rounded-full text-faint transition-colors hover:text-live-ink"
    >
      {children}
    </button>
  );
}
