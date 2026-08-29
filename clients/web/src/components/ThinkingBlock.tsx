import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";

export function ThinkingBlock({ text }: { text: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <div data-testid="thinking-block">
      <button
        className="-mx-1.5 flex items-center gap-2 rounded-[var(--radius-chip)] px-1.5 py-1 transition-colors hover:bg-raised"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        data-testid="thinking-toggle"
      >
        <ChevronRight
          size={11}
          className={cn(
            "shrink-0 text-faint transition-transform",
            open && "rotate-90",
          )}
          aria-hidden
        />
        <span className="legend">{t("thinking.label")}</span>
      </button>
      {open && (
        <pre
          className="screen mt-1.5 ml-[26px] overflow-x-auto px-2.5 py-2 font-mono text-[0.6875rem] leading-relaxed whitespace-pre-wrap text-faint"
          data-testid="thinking-content"
        >
          {text}
        </pre>
      )}
    </div>
  );
}
