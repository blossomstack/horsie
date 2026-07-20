import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { cn } from "../lib/cn";

export function ThinkingBlock({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div data-testid="thinking-block">
      <button
        className="-ml-1 flex items-center gap-1 rounded px-1 py-0.5 text-xs text-faint transition-colors hover:bg-surface-2 hover:text-muted"
        onClick={() => setOpen((o) => !o)}
        data-testid="thinking-toggle"
      >
        <ChevronRight
          size={11}
          className={cn("transition-transform", open && "rotate-90")}
        />
        <span>Thought for a moment</span>
      </button>
      {open && (
        <div
          className="mt-1 ml-3 border-l pl-3 text-xs leading-relaxed whitespace-pre-wrap text-faint"
          data-testid="thinking-content"
        >
          {text}
        </div>
      )}
    </div>
  );
}
