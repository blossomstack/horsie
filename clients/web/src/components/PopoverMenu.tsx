import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

/** A panel selector: a small legend-labelled control that drops its options
 * above itself (it lives on the action row, at the bottom of the screen). */
export function PopoverMenu({
  label,
  legend,
  icon,
  disabled = false,
  testId,
  width = "w-64",
  children,
}: {
  label: ReactNode;
  /** The engraved channel name. Shown above the value so the control reads as
   * a labelled setting rather than an anonymous chip. */
  legend?: string;
  icon?: ReactNode;
  disabled?: boolean;
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

  return (
    <div className="relative" ref={ref}>
      <button
        type="button"
        className={cn(
          "flex items-center gap-1.5 rounded-[var(--radius-control)] px-2 py-1 text-left transition-colors",
          "shadow-[inset_0_0_0_1px_var(--rule)]",
          disabled ? "cursor-default opacity-70" : "hover:bg-raised",
          open && "bg-raised",
        )}
        onClick={() => !disabled && setOpen((o) => !o)}
        disabled={disabled}
        aria-expanded={disabled ? undefined : open}
        data-testid={testId}
      >
        {icon && <span className="text-faint">{icon}</span>}
        <span className="min-w-0">
          {legend && <span className="legend block leading-none">{legend}</span>}
          <span className="block max-w-[11rem] truncate font-mono text-[11px] text-legend">
            {label}
          </span>
        </span>
        {!disabled && (
          <ChevronDown size={12} className="shrink-0 text-faint" aria-hidden />
        )}
      </button>
      {open && !disabled && (
        <div
          className={cn(
            "panel absolute bottom-full left-0 z-20 mb-1.5 max-h-72 overflow-y-auto p-1.5 shadow-[var(--panel-lift)]",
            width,
          )}
        >
          {children(() => setOpen(false))}
        </div>
      )}
    </div>
  );
}
