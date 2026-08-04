import { ChevronDown } from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { cn } from "../lib/cn";

/** Breathing room between a menu and whatever would otherwise clip it. */
const EDGE = 8;

/**
 * The box a menu is actually allowed to occupy.
 *
 * Two constraints, intersected: the nearest `[data-popover-boundary]` — the
 * column the control belongs to — and every ancestor whose overflow would
 * clip it, down to the viewport.
 *
 * The declared boundary is the load-bearing one. Overflow alone is not enough:
 * on a session route the content column has *visible* overflow all the way up
 * to the app shell, whose box is the whole window, so a menu clamped by
 * overflow is clamped to nothing and still runs under the rail on one side and
 * across the plan column on the other. "Fits on screen" was never the
 * requirement; "fits in this column" is.
 */
function clipBounds(from: HTMLElement): { left: number; right: number } {
  let left = 0;
  let right = window.innerWidth;
  const narrow = (el: Element) => {
    const r = el.getBoundingClientRect();
    left = Math.max(left, r.left);
    right = Math.min(right, r.right);
  };
  const boundary = from.closest("[data-popover-boundary]");
  if (boundary) narrow(boundary);
  for (let el = from.parentElement; el; el = el.parentElement) {
    const { overflowX, overflowY } = getComputedStyle(el);
    if (overflowX === "visible" && overflowY === "visible") continue;
    narrow(el);
  }
  return { left, right };
}

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
  const panelRef = useRef<HTMLDivElement>(null);
  // Horizontal correction and the width cap, both measured. The applied shift
  // is mirrored in a ref so `place` can subtract it without depending on the
  // state it sets — otherwise every measurement re-creates the callback and
  // re-runs the effect.
  const [box, setBox] = useState({ shift: 0, maxWidth: 0 });
  const shiftRef = useRef(0);

  /**
   * Anchor the menu to the trigger's left edge, then slide it back inside its
   * column.
   *
   * It used to be right-anchored for every icon key, on the assumption that
   * those keys live at the right end of the row. Once the row grew a left
   * group, a 20rem menu hanging leftward from a key an inch from the pane's
   * edge went straight under the session rail.
   */
  const place = useCallback(() => {
    const anchor = ref.current;
    const panel = panelRef.current;
    if (!anchor || !panel) return;
    const bounds = clipBounds(anchor);
    // Measure from the un-shifted position so the correction never compounds.
    const r = panel.getBoundingClientRect();
    const left = r.left - shiftRef.current;
    const right = r.right - shiftRef.current;
    let dx = 0;
    if (right > bounds.right - EDGE) dx = bounds.right - EDGE - right;
    if (left + dx < bounds.left + EDGE) dx = bounds.left + EDGE - left;
    shiftRef.current = dx;
    setBox({ shift: dx, maxWidth: Math.max(0, bounds.right - bounds.left - EDGE * 2) });
  }, []);

  useLayoutEffect(() => {
    if (!open) {
      shiftRef.current = 0;
      setBox({ shift: 0, maxWidth: 0 });
      return;
    }
    place();
    window.addEventListener("resize", place);
    // Capture phase: the transcript, the settings pane and the rail all scroll
    // in their own containers, and a scroll on any of them moves the trigger
    // out from under a menu measured before it.
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open, place]);

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
            <span className="block truncate font-mono text-[0.6875rem] text-legend">
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
          ref={panelRef}
          className={cn(
            // z-30, not z-20: the overlaid plan panel is also z-20 and later in
            // the DOM, so a tie went to the panel. Its scrim happens to make
            // the config bar unreachable while it is open, but a menu should
            // not depend on that to be on top.
            "panel absolute left-0 z-30 max-h-72 overflow-y-auto p-1.5 shadow-[var(--panel-lift)]",
            // The action row sits at the bottom of the screen, so its menus
            // open upward or they open off-screen; a form's open downward.
            placement === "up" ? "bottom-full mb-1.5" : "top-full mt-1.5",
            width,
          )}
          // Anchored left, then measured back inside its column. A transform
          // rather than a `left` override so the anchor stays declarative and
          // only the correction is imperative — and always emitted, so a menu
          // does not gain and lose a containing block depending on where it
          // happened to land.
          style={{
            transform: `translateX(${box.shift}px)`,
            maxWidth: box.maxWidth || undefined,
          }}
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
