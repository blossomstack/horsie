import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

export function PopoverMenu({
  label,
  icon,
  disabled = false,
  testId,
  width = "w-64",
  children,
}: {
  label: ReactNode;
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
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [open]);

  return (
    <div className="relative" ref={ref}>
      <button
        type="button"
        className={cn(
          "flex items-center gap-1.5 rounded-[var(--radius)] border px-2.5 py-1.5 text-xs font-medium text-text transition-colors",
          disabled ? "cursor-default opacity-70" : "hover:bg-surface-2",
        )}
        onClick={() => !disabled && setOpen((o) => !o)}
        disabled={disabled}
        data-testid={testId}
      >
        {icon}
        <span className="max-w-[12rem] truncate">{label}</span>
        {!disabled && <ChevronDown size={13} className="text-faint" />}
      </button>
      {open && !disabled && (
        <div
          className={cn(
            "card absolute bottom-full left-0 z-20 mb-1.5 max-h-72 overflow-y-auto p-1.5 shadow-lg",
            width,
          )}
        >
          {children(() => setOpen(false))}
        </div>
      )}
    </div>
  );
}
