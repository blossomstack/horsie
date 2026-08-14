import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

/** Where a message stops being something you read and starts being something
 * you scroll past. Roughly a dozen lines at the transcript's body size. */
const DEFAULT_MAX_HEIGHT = 320;

/**
 * Content that clamps to `maxHeight` and offers to open — but only once it
 * actually overflows.
 *
 * A pasted log or a long brief otherwise owns the whole viewport and pushes
 * the reply you came back for off screen. The measurement is the point: the
 * same text is three lines wide and fifteen narrow, and the rail can open or
 * close underneath it, so a character count would clamp the wrong messages.
 */
export function CollapsibleText({
  children,
  maxHeight = DEFAULT_MAX_HEIGHT,
  className,
}: {
  children: ReactNode;
  maxHeight?: number;
  className?: string;
}) {
  const body = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  const [open, setOpen] = useState(false);

  useLayoutEffect(() => {
    const el = body.current;
    if (!el) return;
    // The slack keeps a message that overshoots by a few pixels from earning a
    // control that reveals almost nothing.
    const measure = () => setOverflows(el.scrollHeight > maxHeight + 8);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [maxHeight, children]);

  const clamped = overflows && !open;

  return (
    <div className="relative">
      <div
        ref={body}
        data-testid="collapsible-body"
        className={cn("overflow-hidden", className)}
        style={clamped ? { maxHeight } : undefined}
      >
        {children}
      </div>
      {clamped && (
        // The fade says the text continues; the button says what to do about
        // it. Without the fade a clamp reads as a message that ends mid-word.
        <div
          aria-hidden
          className="pointer-events-none absolute inset-x-0 bottom-0 h-12 rounded-b-[var(--radius-control)] bg-[linear-gradient(to_top,var(--panel-raised),transparent)]"
        />
      )}
      {overflows && (
        <div className="flex justify-end">
          <button
            type="button"
            data-testid="expand-text"
            className="legend relative px-2 py-1 hover:!text-legend"
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? "Less" : "More"}
          </button>
        </div>
      )}
    </div>
  );
}
