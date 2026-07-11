import {
  ChevronRight,
  CircleAlert,
  CircleCheck,
  Loader2,
  Wrench,
} from "lucide-react";
import { useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { cn } from "../lib/cn";

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
    for (const key of ["command", "cmd", "path", "file_path", "query", "pattern", "url"]) {
      const v = obj[key];
      if (typeof v === "string" && v.length > 0) return v;
    }
  }
  return null;
}

export function ToolCallCard({ call }: { call: RenderedToolCall }) {
  const [open, setOpen] = useState(false);
  const preview = inputPreview(call.input);
  const hasOutput = call.output !== undefined && call.output.length > 0;
  const inputStr = stringifyInput(call.input);

  return (
    <div
      className={cn(
        "overflow-hidden rounded-[var(--radius)] border",
        call.isError ? "border-error/40" : "border-border",
      )}
      style={{ background: "var(--surface-2)" }}
    >
      <button
        className="flex w-full items-center gap-2.5 px-3 py-2 text-left"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="text-faint">
          {call.running ? (
            <Loader2 size={14} className="animate-spin text-accent" />
          ) : call.isError ? (
            <CircleAlert size={14} className="text-error" />
          ) : hasOutput ? (
            <CircleCheck size={14} className="text-success" />
          ) : (
            <Wrench size={14} />
          )}
        </span>
        <span className="font-mono text-[13px] font-medium text-text">
          {call.name}
        </span>
        {preview && (
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-faint">
            {preview}
          </span>
        )}
        {!preview && <span className="flex-1" />}
        {call.running && (
          <span className="chip border-0 bg-transparent text-accent">running…</span>
        )}
        <ChevronRight
          size={14}
          className={cn(
            "shrink-0 text-faint transition-transform",
            open && "rotate-90",
          )}
        />
      </button>

      {open && (
        <div className="space-y-2 border-t px-3 py-2.5">
          {inputStr && (
            <div>
              <div className="mb-1 text-[10px] font-semibold tracking-wide text-faint uppercase">
                Input
              </div>
              <pre className="overflow-x-auto rounded-md bg-[var(--surface-3)] p-2 font-mono text-xs leading-relaxed text-muted">
                {inputStr}
              </pre>
            </div>
          )}
          {hasOutput && (
            <div>
              <div className="mb-1 text-[10px] font-semibold tracking-wide text-faint uppercase">
                {call.isError ? "Error" : "Output"}
              </div>
              <pre
                className={cn(
                  "max-h-72 overflow-auto rounded-md p-2 font-mono text-xs leading-relaxed whitespace-pre-wrap",
                  call.isError ? "text-error" : "text-muted",
                )}
                style={{ background: "var(--surface-3)" }}
              >
                {call.output}
              </pre>
            </div>
          )}
          {!hasOutput && !call.running && (
            <div className="text-xs text-faint">No output.</div>
          )}
        </div>
      )}
    </div>
  );
}
