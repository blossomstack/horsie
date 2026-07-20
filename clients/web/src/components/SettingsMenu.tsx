import { Check, Settings } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { SETTINGS, useUiSettings } from "../hooks/useUiSettings";
import { cn } from "../lib/cn";

export function SettingsMenu() {
  const { values, toggle } = useUiSettings();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  return (
    <div className="relative" ref={ref}>
      <button
        className="btn-icon"
        onClick={() => setOpen((o) => !o)}
        title="Display settings"
        aria-label="Display settings"
        data-testid="settings-menu-button"
      >
        <Settings size={17} />
      </button>
      {open && (
        <div
          className="card absolute right-0 top-full z-10 mt-1.5 w-64 p-1.5 shadow-lg"
          data-testid="settings-menu"
        >
          {SETTINGS.map((def) => (
            <button
              key={def.key}
              className="flex w-full items-start gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left hover:bg-surface-2"
              onClick={() => toggle(def.key)}
              data-testid="setting-toggle"
              data-key={def.key}
              data-checked={values[def.key]}
            >
              <span
                className={cn(
                  "mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border",
                  values[def.key] && "border-transparent",
                )}
                style={values[def.key] ? { background: "var(--accent)" } : undefined}
              >
                {values[def.key] && <Check size={12} className="text-accent-fg" />}
              </span>
              <span className="min-w-0">
                <span className="block text-sm text-text">{def.label}</span>
                <span className="block text-xs text-faint">{def.description}</span>
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
