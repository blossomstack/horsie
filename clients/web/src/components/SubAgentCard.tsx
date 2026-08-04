import { Bot, ChevronRight, CircleAlert, CircleCheck } from "lucide-react";
import { useState } from "react";
import type { RenderedSubAgent } from "../hooks/useSessionStream";
import { cn } from "../lib/cn";
import { formatDuration } from "../lib/time";

/** A finished subagent's report, in the same visual grammar as a tool call —
 * because that is what it is to the reader: a piece of machine work the agent
 * set going, not a turn in the conversation. It arrives on the wire inside a
 * user message, and rendering it as one made a delegating session read as if
 * the person kept pasting reports to themselves.
 *
 * Collapsed it is one line; expanded it shows the result the parent was handed.
 * A failure is marked, never left to look like an ordinary finish. */
export function SubAgentCard({ result }: { result: RenderedSubAgent }) {
  const [open, setOpen] = useState(false);
  const failed = result.status === "failed";
  const hasText = result.text.length > 0;
  // Both stamps or none: a subagent journaled before spans were recorded shows
  // no duration rather than one measured from a zero.
  const duration =
    result.spawnedAtMs > 0 && result.endedAtMs > 0
      ? formatDuration(result.endedAtMs - result.spawnedAtMs)
      : null;

  const row = (
    <>
      <span className="flex w-3.5 shrink-0 justify-center">
        {failed ? (
          <CircleAlert size={12} className="text-red-ink" aria-hidden />
        ) : result.status === "completed" ? (
          <CircleCheck size={12} className="text-lamp-ok" aria-hidden />
        ) : (
          // An unrecognized status borrows neither success nor failure.
          <Bot size={12} className="text-faint" aria-hidden />
        )}
      </span>
      <span className="shrink-0 font-mono text-[0.6875rem] font-medium tracking-[0.02em] text-legend">
        subagent
      </span>
      <span className="min-w-0 flex-1 truncate font-mono text-[0.6875rem] text-faint">
        {result.label}
      </span>
      {failed && <span className="legend shrink-0 !text-red-ink">Failed</span>}
      {duration && (
        <span className="legend shrink-0" data-testid="subagent-duration">
          {duration}
        </span>
      )}
    </>
  );

  return (
    <div
      data-testid="subagent-card"
      data-subagent={result.label}
      data-status={result.status}
    >
      {hasText ? (
        <button
          className="-mx-1.5 flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-1.5 py-1 text-left transition-colors hover:bg-raised"
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
          data-testid="subagent-toggle"
        >
          <ChevronRight
            size={11}
            className={cn(
              "shrink-0 text-faint transition-transform",
              open && "rotate-90",
            )}
            aria-hidden
          />
          {row}
        </button>
      ) : (
        // Nothing to disclose: a control that opens an empty panel is a lie
        // about there being more to read.
        <div className="-mx-1.5 flex w-full items-center gap-2 px-1.5 py-1">
          <span className="w-[11px] shrink-0" aria-hidden />
          {row}
        </div>
      )}

      {open && hasText && (
        <div className="mt-1.5 space-y-1.5 pl-[26px]">
          <span className={cn("legend", failed && "!text-red-ink")}>
            {failed ? "Error" : "Result"}
          </span>
          <pre
            data-testid="subagent-result"
            className={cn(
              "screen mt-1 max-h-72 overflow-auto px-2.5 py-2 font-mono text-[0.6875rem] leading-relaxed whitespace-pre-wrap",
              failed ? "text-red-ink" : "text-dim",
            )}
          >
            {result.text}
          </pre>
        </div>
      )}
    </div>
  );
}
