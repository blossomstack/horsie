import { Check, SlidersHorizontal } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useEffect, useRef, useState } from "react";
import { SETTINGS, useUiSettings } from "../hooks/useUiSettings";
import { cn } from "../lib/cn";

/** Display switches for this browser — what the panel shows, not what the
 * session does. Kept separate from Settings for exactly that reason. */
export function SettingsMenu({ disabled }: { disabled?: boolean } = {}) {
  const { t } = useTranslation();
  const { values, toggle } = useUiSettings();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="relative" ref={ref}>
      <button
        className="key-icon"
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
        title={t("settingsMenu.title")}
        aria-label={t("settingsMenu.ariaLabel")}
        aria-expanded={open}
        data-testid="settings-menu-button"
      >
        <SlidersHorizontal size={15} aria-hidden />
      </button>
      {open && (
        <div
          className="panel absolute right-0 top-full z-10 mt-2 w-64 p-1.5 shadow-[var(--float)]"
          data-testid="settings-menu"
        >
          <p className="legend px-2 pb-1 pt-1">{t("settingsMenu.heading")}</p>
          {SETTINGS.map((def) => (
            <button
              key={def.key}
              className="flex w-full items-start gap-2.5 rounded-[var(--radius-chip)] px-2 py-1.5 text-left transition-colors hover:bg-raised"
              onClick={() => toggle(def.key)}
              role="switch"
              aria-checked={values[def.key]}
              data-testid="setting-toggle"
              data-key={def.key}
              data-checked={values[def.key]}
            >
              <span
                aria-hidden
                className={cn(
                  "mt-px flex h-4 w-4 shrink-0 items-center justify-center rounded-[3px]",
                  values[def.key]
                    ? "bg-accent text-accent-ink"
                    : "shadow-[inset_0_0_0_1px_var(--rule-strong)]",
                )}
              >
                {values[def.key] && <Check size={11} strokeWidth={3} />}
              </span>
              <span className="min-w-0 text-[0.8125rem] text-legend">
                {t(`ui.${def.key}.label`)}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
