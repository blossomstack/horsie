import { ArrowUp, Square } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";

export function Composer({
  status,
  busy,
  blockedReason = null,
  askLocked = false,
  showStop = false,
  onSend,
  onStop,
  onFocusAsk,
}: {
  status: SessionStatusKind;
  busy: boolean;
  blockedReason?: string | null;
  /** A question in the transcript is awaiting an answer (or one is in flight).
   * The ask card owns the input, so the composer stands down — two live input
   * surfaces would make it ambiguous which one a Send submits. */
  askLocked?: boolean;
  /** Show Stop even when the status isn't `Running`: a turn resumed from an ask
   * stays `AwaitingInput` for its whole duration. */
  showStop?: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
  onFocusAsk?: () => void;
}) {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);
  const meta = statusMeta(status);
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;
  const blocked = blockedReason != null;
  const stoppable = running || showStop;

  // Auto-grow the textarea up to a cap.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  const submit = () => {
    const trimmed = text.trim();
    if (!trimmed || !meta.canSend || busy || blocked || askLocked) return;
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
            askLocked
              ? "Answer the question above"
              : meta.canSend
                ? awaiting
                  ? "Answer the agent…"
                  : "Send a message…  (Enter to send, Shift+Enter for newline)"
                : meta.hint
          }
          disabled={askLocked || (!meta.canSend && !running)}
          className="max-h-[200px] flex-1 resize-none bg-transparent px-2 py-1.5 text-[0.9375rem] text-text placeholder:text-faint outline-none disabled:opacity-60"
        />

        {stoppable ? (
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
            disabled={!text.trim() || !meta.canSend || busy || blocked || askLocked}
            title={blockedReason ?? "Send"}
            aria-label="Send message"
            data-testid="composer-send"
          >
            <ArrowUp size={18} />
          </button>
        )}
      </div>

      {askLocked && (
        <button
          type="button"
          onClick={onFocusAsk}
          data-testid="composer-ask-hint"
          className="mt-1.5 px-2 text-xs text-faint hover:text-muted"
        >
          Answer the question above
        </button>
      )}

      {blocked && (
        <p
          className="mt-1.5 px-2 text-xs text-faint"
          data-testid="composer-blocked-hint"
        >
          {blockedReason}
        </p>
      )}
    </div>
  );
}
