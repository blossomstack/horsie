import { ArrowUp, HelpCircle, Square } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";

export function Composer({
  status,
  pendingQuestion,
  busy,
  onSend,
  onStop,
}: {
  status: SessionStatusKind;
  pendingQuestion: string | null;
  busy: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
}) {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);
  const meta = statusMeta(status);
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;

  // Auto-grow the textarea up to a cap.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  const submit = () => {
    const trimmed = text.trim();
    if (!trimmed || !meta.canSend || busy) return;
    onSend(trimmed);
    setText("");
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div className="mx-auto w-full max-w-3xl px-4 pb-4">
      {awaiting && pendingQuestion && (
        <div
          data-testid="ask-question-banner"
          className="mb-2 flex items-start gap-2 rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-text"
        >
          <HelpCircle size={16} className="mt-0.5 shrink-0 text-warning" />
          <div>
            <span className="font-medium text-warning">Agent is asking:</span>{" "}
            {pendingQuestion}
          </div>
        </div>
      )}

      <div
        className={cn(
          "flex items-end gap-2 rounded-[var(--radius-lg)] border p-2 transition",
          "focus-within:border-accent",
        )}
        style={{ background: "var(--surface)" }}
      >
        <textarea
          ref={ref}
          rows={1}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          data-testid="composer-input"
          placeholder={
            meta.canSend
              ? awaiting
                ? "Answer the agent…"
                : "Send a message…  (Enter to send, Shift+Enter for newline)"
              : meta.hint
          }
          disabled={!meta.canSend && !running}
          className="max-h-[200px] flex-1 resize-none bg-transparent px-2 py-1.5 text-[0.9375rem] text-text placeholder:text-faint outline-none disabled:opacity-60"
        />

        {running ? (
          <button
            className="btn-outline shrink-0"
            onClick={onStop}
            disabled={busy}
            title="Stop the session (preserves the runtime)"
            data-testid="composer-stop"
          >
            <Square size={15} className="fill-current" />
            Stop
          </button>
        ) : (
          <button
            className="btn-primary shrink-0 !px-3"
            onClick={submit}
            disabled={!text.trim() || !meta.canSend || busy}
            title="Send"
            aria-label="Send message"
            data-testid="composer-send"
          >
            <ArrowUp size={18} />
          </button>
        )}
      </div>
    </div>
  );
}
