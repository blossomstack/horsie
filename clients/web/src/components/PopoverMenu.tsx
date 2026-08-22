import { ChevronDown } from "lucide-react";
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { useReturnFocus } from "../hooks/useReturnFocus";
import { cn } from "../lib/cn";

/** Breathing room between a menu and whatever would otherwise clip it. */
const EDGE = 8;
/** Never cap a menu below this: a sliver that shows no option is worse than
 * one that overhangs slightly. */
const MIN_PANEL_HEIGHT = 160;

/**
 * Marks a button that behaves as one choice in a list of choices.
 *
 * Opt-in, because a panel's body is arbitrary: some are checklists of real
 * checkboxes, one is a real radio group, several are read-only summaries, and
 * all of those already have the keyboard behaviour their native control
 * defines. Only the hand-rolled `<button>` lists — model, environment,
 * workflow — need this component to supply arrow keys and a single tab stop,
 * and only they carry the attribute.
 */
const OPTION_SELECTOR = "[data-popover-option]:not([disabled])";

/** Give one option the tab stop and the focus; the rest step out of the way. */
function focusOption(options: HTMLElement[], i: number) {
  options.forEach((el, j) => {
    el.tabIndex = j === i ? 0 : -1;
  });
  options[i]?.focus();
}

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
  /** Overrides the dot's colour to live: the control is reachable but the
   * thing it configures is in a state the operator should look at. */
  warn = false,
  testId,
  width = "w-64",
  height,
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
  /** Tailwind max-height for the panel; defaults to `max-h-72`. */
  height?: string;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelId = useId();
  // Set when the panel was opened from the keyboard, which is the one case
  // where it should take the focus with it.
  const grabFocus = useRef(false);
  // Horizontal correction and the width cap, both measured. The applied shift
  // is mirrored in a ref so `place` can subtract it without depending on the
  // state it sets — otherwise every measurement re-creates the callback and
  // re-runs the effect.
  const [box, setBox] = useState({ shift: 0, maxWidth: 0, maxHeight: 0 });
  const shiftRef = useRef(0);

  /**
   * Anchor the menu to the trigger's left edge, slide it back inside its
   * column, and cap its height at the room it actually has.
   *
   * It used to be right-anchored for every icon key, on the assumption that
   * those keys live at the right end of the row. Once the row grew a left
   * group, a 20rem menu hanging leftward from a key an inch from the pane's
   * edge went straight under the session rail.
   *
   * The vertical cap is the same idea one axis over, and it is not optional
   * for a tall menu: the action row sits at the bottom of the window, so its
   * menus open *upward*, and a fixed max-height taller than the space above
   * the trigger runs off the top of the screen — taking its own header and
   * quick actions with it, which is the half you cannot scroll back to.
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
    // Room between the trigger and the edge of the window it opens toward.
    const a = anchor.getBoundingClientRect();
    const room =
      placement === "up" ? a.top - EDGE : window.innerHeight - a.bottom - EDGE;
    setBox({
      shift: dx,
      maxWidth: Math.max(0, bounds.right - bounds.left - EDGE * 2),
      // A floor, so a trigger that happens to sit near an edge gets a usable
      // menu that scrolls rather than a sliver that cannot show one option.
      maxHeight: Math.max(MIN_PANEL_HEIGHT, room),
    });
  }, [placement]);

  useLayoutEffect(() => {
    if (!open) {
      shiftRef.current = 0;
      setBox({ shift: 0, maxWidth: 0, maxHeight: 0 });
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

  useReturnFocus(open, triggerRef);

  const optionList = useCallback(
    () =>
      Array.from(
        panelRef.current?.querySelectorAll<HTMLElement>(OPTION_SELECTOR) ?? [],
      ),
    [],
  );

  // Roving tab stop. One option is tabbable so Tab out of the trigger lands on
  // the current choice rather than on each of them in turn; the arrow keys
  // move between them. No dependency list on purpose — a panel's options are
  // re-rendered whenever the draft changes underneath it, and a freshly
  // mounted <button> is tabbable again until this says otherwise.
  useEffect(() => {
    if (!open) return;
    const options = optionList();
    if (options.length === 0) return;
    const held = options.findIndex((el) => el.getAttribute("tabindex") === "0");
    const chosen = options.findIndex(
      (el) => el.getAttribute("aria-pressed") === "true",
    );
    const at = held >= 0 ? held : chosen >= 0 ? chosen : 0;
    options.forEach((el, i) => {
      el.tabIndex = i === at ? 0 : -1;
    });
  });

  useEffect(() => {
    if (!open || !grabFocus.current) return;
    grabFocus.current = false;
    const options = optionList();
    if (options.length > 0) focusOption(options, 0);
  }, [open, optionList]);

  const onPanelKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    const options = optionList();
    if (options.length === 0) return;
    const at = options.indexOf(document.activeElement as HTMLElement);
    const last = options.length - 1;
    let next: number;
    if (e.key === "ArrowDown") next = at < 0 ? 0 : (at + 1) % options.length;
    else if (e.key === "ArrowUp") next = at <= 0 ? last : at - 1;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = last;
    else return;
    e.preventDefault();
    focusOption(options, next);
  };

  const onTriggerKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (disabled || open || e.key !== "ArrowDown") return;
    e.preventDefault();
    grabFocus.current = true;
    setOpen(true);
  };

  // Both the tooltip and the accessible name. An icon-only control that only
  // says "Model" tells you which control it is but not what it is set to,
  // which is the half that matters at a glance.
  const described = legend ? `${legend} — ${labelText(label)}` : labelText(label);

  // The panel holds arbitrary form content — checklists, a radio group, a
  // read-only summary — so it is a non-modal dialog rather than a menu, whose
  // children would all have to be menu items.
  const triggerAria = {
    "aria-expanded": disabled ? undefined : open,
    "aria-haspopup": disabled ? undefined : ("dialog" as const),
    "aria-controls": open && !disabled ? panelId : undefined,
  };

  return (
    <div className={cn("relative", className)} ref={ref}>
      {variant === "icon" ? (
        <button
          ref={triggerRef}
          type="button"
          className={cn(
            "key-icon",
            // State is the control's own colour, not a badge stuck on it. A
            // dot in the corner of a 2rem key is four pixels doing the work of
            // a whole control, and accent there competes with the one accent
            // key that actually commits.
            warn
              ? "!bg-live-quiet !text-live-ink"
              : marked
                ? "bg-raised !text-legend"
                : "!text-faint",
            disabled && "cursor-default opacity-70",
            open && "bg-raised !text-legend",
          )}
          onClick={() => !disabled && setOpen((o) => !o)}
          onKeyDown={onTriggerKeyDown}
          disabled={disabled}
          {...triggerAria}
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
          ref={triggerRef}
          type="button"
          className={cn(
            "flex w-full items-center gap-1.5 rounded-[var(--radius-control)] px-2 py-1 text-left transition-colors",
            disabled ? "cursor-default opacity-70" : "hover:bg-raised",
            open && "bg-raised",
          )}
          onClick={() => !disabled && setOpen((o) => !o)}
          onKeyDown={onTriggerKeyDown}
          disabled={disabled}
          {...triggerAria}
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
          id={panelId}
          role="dialog"
          aria-label={described || undefined}
          onKeyDown={onPanelKeyDown}
          className={cn(
            // z-30, not z-20: the overlaid plan panel is also z-20 and later in
            // the DOM, so a tie went to the panel. Its scrim happens to make
            // the config bar unreachable while it is open, but a menu should
            // not depend on that to be on top.
            "panel absolute left-0 z-30 overflow-y-auto p-1.5 shadow-[var(--float)]",
            // 18rem suits a list of bare names. A picker whose options each
            // carry a description and a badge says so — see `PickerSpec.height`
            // — because a two-line option in an 18rem box shows four of them
            // and hides the rest behind a scrollbar nobody expects.
            height ?? "max-h-72",
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
            // Narrows the class cap, never widens it: `max-h-*` and this both
            // apply, so the smaller wins and a short menu keeps its own size.
            maxHeight: box.maxHeight || undefined,
          }}
        >
          {variant === "icon" && legend && (
            <p className="legend px-1 pb-1.5">{legend}</p>
          )}
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
