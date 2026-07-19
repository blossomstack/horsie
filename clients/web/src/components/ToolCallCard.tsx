import {
  ChevronRight,
  CircleAlert,
  CircleCheck,
  HelpCircle,
  Loader2,
  Wrench,
} from "lucide-react";
import { useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { cn } from "../lib/cn";

/** The server's dedicated "ask the user" tool for sessions (see
 * server/src/sessions/ask_tool.rs) — rendered as a question, not a generic
 * tool call. */
const ASK_USER_TOOL = "ask_user";

function AskUserCard({ call }: { call: RenderedToolCall }) {
  const input = call.input as { question?: string; choices?: string[] } | undefined;
  const choices = input?.choices ?? [];
  return (
    <div
      data-testid="ask-user-card"
      className="flex items-start gap-2 rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-text"
    >
      <HelpCircle size={16} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 flex-1">
        <span className="font-medium text-warning">Asked: </span>
        {input?.question ?? ""}
        {choices.length > 0 && (
          <div className="mt-1.5 flex flex-wrap gap-1.5">
            {choices.map((c) => (
              <span key={c} data-testid="ask-user-choice" className="chip">
                {c}
              </span>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

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
  if (call.name === ASK_USER_TOOL) return <AskUserCard call={call} />;
  const preview = inputPreview(call.input);
  const hasOutput = call.output !== undefined && call.output.length > 0;
  const inputStr = stringifyInput(call.input);

  return (
    <div data-testid="tool-call-card" data-tool={call.name} data-error={call.isError ? "true" : "false"}>
      <button
        className="-mx-1 flex w-full items-center gap-2 rounded px-1 py-1 text-left hover:bg-surface-2"
        onClick={() => setOpen((o) => !o)}
        data-testid="tool-call-toggle"
      >
        <ChevronRight
          size={11}
          className={cn(
            "shrink-0 text-faint transition-transform",
            open && "rotate-90",
          )}
        />
        <span className="shrink-0 text-faint">
          {call.running ? (
            <Loader2 size={13} className="animate-spin text-accent" />
          ) : call.isError ? (
            <CircleAlert size={13} className="text-error" />
          ) : hasOutput ? (
            <CircleCheck size={13} className="text-success" />
          ) : (
            <Wrench size={13} />
          )}
        </span>
        <span className="font-mono text-[13px] text-text">{call.name}</span>
        {preview && (
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-faint">
            {preview}
          </span>
        )}
        {!preview && <span className="flex-1" />}
        {call.running && (
          <span className="shrink-0 text-xs text-accent">running…</span>
        )}
      </button>

      {open && (
        <div className="mt-1 ml-3 space-y-2 border-l pl-3">
          {inputStr && (
            <pre className="overflow-x-auto font-mono text-xs leading-relaxed whitespace-pre-wrap text-faint">
              {inputStr}
            </pre>
          )}
          {hasOutput && (
            <pre
              data-testid="tool-call-output"
              className={cn(
                "max-h-72 overflow-auto font-mono text-xs leading-relaxed whitespace-pre-wrap",
                call.isError ? "text-error" : "text-muted",
              )}
            >
              {call.output}
            </pre>
          )}
          {!hasOutput && !call.running && (
            <div className="text-xs text-faint">No output.</div>
          )}
        </div>
      )}
    </div>
  );
}
