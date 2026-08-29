import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
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
  const { t } = useTranslation();
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
    // The control lives *inside* the block it opens, bottom right, over the
    // fade. Below it, it read as a detached strip under the message rather than
    // part of it. `paddingBottom` is inline because it has to beat whatever
    // padding the caller's class list already sets — that is a specificity
    // fight a utility class does not reliably win.
    <div className="relative">
      <div
        ref={body}
        data-testid="collapsible-body"
        className={cn("overflow-hidden", className)}
        style={{
          maxHeight: clamped ? maxHeight : undefined,
          paddingBottom: overflows ? "1.75rem" : undefined,
        }}
      >
        {children}
      </div>
      {clamped && (
        // The fade says the text continues. Without it a clamp reads as a
        // message that ends mid-word.
        <div
          aria-hidden
          className="pointer-events-none absolute inset-x-px bottom-px h-14 rounded-b-[var(--radius-control)] bg-[linear-gradient(to_top,var(--panel-raised),transparent)]"
        />
      )}
      {overflows && (
        <button
          type="button"
          data-testid="expand-text"
          className="legend absolute bottom-1.5 right-3 px-1.5 py-0.5 hover:!text-legend"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
        >
          {open ? t("common.less") : t("common.more")}
        </button>
      )}
    </div>
  );
}
