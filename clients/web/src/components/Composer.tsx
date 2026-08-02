import { ArrowUp, Square } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";

export function Composer({
  status,
  busy,
  blockedReason = null,
  askPending = false,
  onSend,
  onStop,
  onFocusAsk,
}: {
  status: SessionStatusKind | null | undefined;
  busy: boolean;
  blockedReason?: string | null;
  /** A question in the transcript is awaiting an answer. The composer stays
   * live — sending from here answers it, exactly as the card does — but says
   * where the question is. */
  askPending?: boolean;
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

  // Auto-grow the textarea up to a cap.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  const submit = () => {
    const trimmed = text.trim();
    if (!trimmed || !meta.canSend || busy || blocked) return;
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
            !meta.canSend
              ? meta.hint
              : awaiting
                ? "Answer the agent…"
                : running
                  ? "Send a message… it goes in with the next turn"
                  : "Send a message…  (Enter to send, Shift+Enter for newline)"
          }
          disabled={!meta.canSend}
          className="max-h-[200px] flex-1 resize-none bg-transparent px-2 py-1.5 text-[0.9375rem] text-text placeholder:text-faint outline-none disabled:opacity-60"
        />

        {/* Stop sits *beside* Send, never instead of it: a turn in flight is
            exactly when queueing the next message is most useful. */}
        {running && (
          <button
            className="btn-outline shrink-0"
            onClick={onStop}
            disabled={busy}
            title="Stop this turn (queued messages are kept)"
            data-testid="composer-stop"
          >
            <Square size={15} className="fill-current" />
            Stop
          </button>
        )}
        <button
          className="btn-primary shrink-0 !px-3"
          onClick={submit}
          disabled={!text.trim() || !meta.canSend || busy || blocked}
          title={blockedReason ?? (running ? "Queue for the next turn" : "Send")}
          aria-label="Send message"
          data-testid="composer-send"
        >
          <ArrowUp size={18} />
        </button>
      </div>

      {askPending && (
        <button
          type="button"
          onClick={onFocusAsk}
          data-testid="composer-ask-hint"
          className="mt-1.5 px-2 text-xs text-faint hover:text-muted"
        >
          Jump to the question
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
