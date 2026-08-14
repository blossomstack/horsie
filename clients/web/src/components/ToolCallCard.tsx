import {
  ChevronRight,
  CircleAlert,
  CircleCheck,
  ShieldBan,
  ShieldCheck,
  Wrench,
} from "lucide-react";
import { useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { isAskCall } from "../lib/askUser";
import { cn } from "../lib/cn";
import { deniesCall, hookSummary, systemMessage } from "../lib/hookSummary";
import { AskUserCard } from "./AskUserCard";

function stringifyInput(input: unknown): string {
  if (input == null) return "";
  if (typeof input === "string") return input;
  try {
    return JSON.stringify(input, null, 2);
  } catch {
    return String(input);
  }
}

/** One-line hint from the most salient input field (command, path, query…). */
function inputPreview(input: unknown): string | null {
  if (input == null) return null;
  if (typeof input === "string") return input;
  if (typeof input === "object") {
    const obj = input as Record<string, unknown>;
    for (const key of [
      "command",
      "cmd",
      "path",
      "file_path",
      "query",
      "pattern",
      "url",
    ]) {
      const v = obj[key];
      if (typeof v === "string" && v.length > 0) return v;
    }
  }
  return null;
}

/** A logged tool call. Collapsed it is one line of the recording; expanded it
 * shows the raw input and output on recessed screens, because these operators
 * came to read exactly what the machine sent and got back. */
export function ToolCallCard({ call }: { call: RenderedToolCall }) {
  const [open, setOpen] = useState(false);
  if (isAskCall(call.name)) return <AskUserCard call={call} />;
  const preview = inputPreview(call.input);
  const hasOutput = call.output !== undefined && call.output.length > 0;
  // A denial the model saw as an error is the one hook outcome that changes what
  // the row means, so it is named on the collapsed line.
  const blocked = call.hooks.find(deniesCall);
  const inputStr = stringifyInput(call.input);

  return (
    <div
      data-testid="tool-call-card"
      data-tool={call.name}
      data-error={call.isError ? "true" : "false"}
    >
      <button
        className="-mx-1.5 flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-1.5 py-1 text-left transition-colors hover:bg-raised"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        data-testid="tool-call-toggle"
      >
        <ChevronRight
          size={11}
          className={cn(
            "shrink-0 text-faint transition-transform",
            open && "rotate-90",
          )}
          aria-hidden
        />
        <span className="flex w-3.5 shrink-0 justify-center">
          {call.running ? (
            <span className="lamp lamp-live text-amber-ink" aria-hidden />
          ) : call.isError ? (
            <CircleAlert size={12} className="text-red-ink" aria-hidden />
          ) : hasOutput ? (
            <CircleCheck size={12} className="text-lamp-ok" aria-hidden />
          ) : (
            <Wrench size={12} className="text-faint" aria-hidden />
          )}
        </span>
        <span className="shrink-0 font-mono text-[0.6875rem] font-medium tracking-[0.02em] text-legend">
          {call.name}
        </span>
        {preview && (
          <span className="min-w-0 flex-1 truncate font-mono text-[0.6875rem] text-faint">
            {preview}
          </span>
        )}
        {!preview && <span className="flex-1" />}
        {call.running && <span className="legend shrink-0">Running</span>}
        {call.isError && !call.running && (
          <span className="legend shrink-0 !text-red-ink">Failed</span>
        )}
        {/* A hook that changed the call is state the collapsed row must carry:
            "this is not what the agent asked for" cannot wait behind a toggle.
            A hook that merely watched is detail, and stays inside. */}
        {blocked && (
          <span
            className="legend shrink-0 !text-red-ink"
            data-testid="tool-call-hook-blocked"
          >
            Blocked by {blocked.plugin}
          </span>
        )}
      </button>

      {open && (
        <div className="mt-1.5 space-y-1.5 pl-[26px]">
          {inputStr && (
            <div>
              <span className="legend">Input</span>
              <pre className="screen mt-1 overflow-x-auto px-2.5 py-2 font-mono text-[0.6875rem] leading-relaxed whitespace-pre-wrap text-dim">
                {inputStr}
              </pre>
            </div>
          )}
          {hasOutput && (
            <div>
              <span className={cn("legend", call.isError && "!text-red-ink")}>
                {call.isError ? "Error" : "Output"}
              </span>
              <pre
                data-testid="tool-call-output"
                className={cn(
                  "screen mt-1 max-h-72 overflow-auto px-2.5 py-2 font-mono text-[0.6875rem] leading-relaxed whitespace-pre-wrap",
                  call.isError ? "text-red-ink" : "text-dim",
                )}
              >
                {call.output}
              </pre>
            </div>
          )}
          {!hasOutput && !call.running && (
            <p className="legend">Returned nothing</p>
          )}
          {call.hooks.length > 0 && (
            <div data-testid="tool-call-hooks">
              <span className="legend">Plugin hooks</span>
              <ul className="mt-1 space-y-1">
                {call.hooks.map((h, i) => {
                  const { text, intervened } = hookSummary(h);
                  const note = systemMessage(h);
                  return (
                    <li
                      key={`${h.plugin}:${h.action.event}:${i}`}
                      className="font-mono text-[0.6875rem] leading-relaxed"
                      data-testid="tool-call-hook"
                    >
                      <div className="flex items-start gap-1.5">
                        {intervened ? (
                          <ShieldBan
                            size={12}
                            className="mt-px shrink-0 text-red-ink"
                            aria-hidden
                          />
                        ) : (
                          <ShieldCheck
                            size={12}
                            className="mt-px shrink-0 text-lamp-ok"
                            aria-hidden
                          />
                        )}
                        <span className="text-legend">{h.plugin}</span>
                        <span className="text-faint">{h.action.event}</span>
                        <span
                          className={cn(
                            "min-w-0 flex-1",
                            intervened ? "text-red-ink" : "text-dim",
                          )}
                        >
                          {text}
                        </span>
                      </div>
                      {/* Addressed to the user, not the model — captured since
                          #140 and shown to nobody until now. */}
                      {note && (
                        <p className="mt-0.5 pl-[18px] text-amber-ink">{note}</p>
                      )}
                    </li>
                  );
                })}
              </ul>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
