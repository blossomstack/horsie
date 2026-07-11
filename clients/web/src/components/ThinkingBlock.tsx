import { Brain, ChevronRight } from "lucide-react";
import { useState } from "react";
import { cn } from "../lib/cn";

export function ThinkingBlock({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="rounded-[var(--radius)] border border-dashed">
      <button
        className="flex w-full items-center gap-2 px-3 py-2 text-xs font-medium text-faint transition-colors hover:text-muted"
        onClick={() => setOpen((o) => !o)}
      >
        <Brain size={13} />
        <span>Thinking</span>
        <ChevronRight
          size={13}
          className={cn("ml-auto transition-transform", open && "rotate-90")}
        />
      </button>
      {open && (
        <div className="border-t border-dashed px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap text-muted">
          {text}
        </div>
      )}
    </div>
  );
}
