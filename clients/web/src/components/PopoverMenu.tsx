import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

/**
 * A panel selector.
 *
 * Two renditions of the same control, because it serves two surfaces. On the
 * session action row it is `icon` — a bare key with a dot when it holds a
 * value, since a row of seven labelled controls wrapped onto three lines and
 * spent more height than the transcript could afford. In a form it is `field`,
 * labelled and full-width, because a form is read top to bottom and its rows
 * are supposed to name themselves.
 */
export function PopoverMenu({
  label,
  legend,
  icon,
  variant = "field",
  placement = "up",
  className,
  disabled = false,
  /** Something other than the default is selected — draws the dot in `icon`. */
  marked = false,
  /** Overrides the dot's colour to amber: the control is reachable but the
   * thing it configures is in a state the operator should look at. */
  warn = false,
  testId,
  width = "w-64",
  children,
}: {
  label: ReactNode;
  /** The engraved channel name. In `icon` it is not rendered, but it still
   * carries the accessible name and the tooltip — losing the visible label
   * must not mean losing the label. */
  legend?: string;
  icon?: ReactNode;
  variant?: "field" | "icon";
  placement?: "up" | "down";
  /** Layout only — positioning within the row that renders it. */
  className?: string;
  disabled?: boolean;
  marked?: boolean;
  warn?: boolean;
  testId?: string;
  width?: string;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Both the tooltip and the accessible name. An icon-only control that only
  // says "Model" tells you which control it is but not what it is set to,
  // which is the half that matters at a glance.
  const described = legend ? `${legend} — ${labelText(label)}` : labelText(label);

  return (
    <div className={cn("relative", className)} ref={ref}>
      {variant === "icon" ? (
        <button
          type="button"
          className={cn(
            "key-icon",
            // State is the control's own colour, not a badge stuck on it. A
            // dot in the corner of a 2rem key is four pixels doing the work of
            // a whole control, and orange there competes with the one orange
            // key that actually commits.
            warn
              ? "!bg-amber-quiet !text-amber-ink"
              : marked
                ? "bg-raised !text-legend"
                : "!text-faint",
            disabled && "cursor-default opacity-70",
            open && "bg-raised !text-legend",
          )}
          onClick={() => !disabled && setOpen((o) => !o)}
          disabled={disabled}
          aria-expanded={disabled ? undefined : open}
          title={described}
          aria-label={described}
          data-testid={testId}
          data-marked={marked ? "true" : undefined}
          data-warn={warn ? "true" : undefined}
        >
          {icon}
        </button>
      ) : (
        <button
          type="button"
          className={cn(
            "flex w-full items-center gap-1.5 rounded-[var(--radius-control)] px-2 py-1 text-left transition-colors",
            "shadow-[inset_0_0_0_1px_var(--row-ring)]",
            disabled ? "cursor-default opacity-70" : "hover:bg-raised",
            open && "bg-raised",
          )}
          onClick={() => !disabled && setOpen((o) => !o)}
          disabled={disabled}
          aria-expanded={disabled ? undefined : open}
          data-testid={testId}
        >
          {icon && <span className="text-faint">{icon}</span>}
          <span className="min-w-0 flex-1">
            {legend && <span className="legend block leading-none">{legend}</span>}
            <span className="block truncate font-mono text-[11px] text-legend">
              {label}
            </span>
          </span>
          {!disabled && (
            <ChevronDown size={12} className="shrink-0 text-faint" aria-hidden />
          )}
        </button>
      )}
      {open && !disabled && (
        <div
          className={cn(
            "panel absolute z-20 max-h-72 overflow-y-auto p-1.5 shadow-[var(--panel-lift)]",
            // The action row sits at the bottom of the screen, so its menus
            // open upward or they open off-screen; a form's open downward.
            placement === "up" ? "bottom-full mb-1.5" : "top-full mt-1.5",
            // Right-anchored in `icon`: these keys sit at the right edge of
            // the action row, and a left-anchored menu overflowed the viewport.
            variant === "icon" ? "right-0" : "left-0",
            width,
          )}
        >
          {children(() => setOpen(false))}
        </div>
      )}
    </div>
  );
}

/** Best-effort text for a label that is usually a string but is typed as a
 * node. Only used for the tooltip and accessible name. */
function labelText(label: ReactNode): string {
  return typeof label === "string" || typeof label === "number"
    ? String(label)
    : "";
}
