import { ArrowUp, Square } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { SessionStatusKind } from "../api/types";
import { statusMeta } from "../lib/status";

/** The action row. One orange key commits; the emergency stop beside it is the
 * only red control on the panel. Both are labelled — an unlabelled icon is a
 * control you have to learn. */
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
    <div className="mx-auto w-full max-w-[54rem] px-4 pb-4 sm:px-6">
      {/* The focus ring rides the whole panel. Setting `border-color` instead
          landed the amber on the internal divider and left the outer edge
          grey, which read as a stray underline. */}
      <div className="panel overflow-hidden transition-shadow focus-within:border-amber focus-within:shadow-[0_0_0_3px_var(--focus-ring)]">
        <textarea
          ref={ref}
          rows={1}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          data-testid="composer-input"
          aria-label="Message the agent"
          placeholder={
            !meta.canSend
              ? meta.hint
              : awaiting
                ? "Answer the agent…"
                : running
                  ? "Queue a message for the next turn…"
                  : "Message the agent…"
          }
          disabled={!meta.canSend}
          className="max-h-[200px] w-full resize-none bg-transparent px-3.5 pb-2 pt-3 text-[0.9375rem] leading-relaxed text-legend outline-none placeholder:text-faint disabled:opacity-60"
        />

        <div className="flex items-center gap-2 border-t px-2.5 py-2">
          <span className="legend hidden sm:block">
            {running ? "Queued for the next turn" : "Enter sends · Shift+Enter newline"}
          </span>

          {/* Stop sits *beside* Send, never instead of it: a turn in flight is
              exactly when queueing the next message is most useful. */}
          <div className="ml-auto flex items-center gap-2">
            {running && (
              <button
                className="key key-stop"
                onClick={onStop}
                disabled={busy}
                title="Stop this turn (queued messages are kept)"
                data-testid="composer-stop"
              >
                <Square size={12} className="fill-current" aria-hidden />
                Stop
              </button>
            )}
            <button
              className="key key-go"
              onClick={submit}
              disabled={!text.trim() || !meta.canSend || busy || blocked}
              title={blockedReason ?? (running ? "Queue for the next turn" : "Send")}
              data-testid="composer-send"
            >
              Send
              <ArrowUp size={13} aria-hidden />
            </button>
          </div>
        </div>
      </div>

      {askPending && (
        <button
          type="button"
          onClick={onFocusAsk}
          data-testid="composer-ask-hint"
          className="mt-2 flex items-center gap-1.5 font-mono text-[10px] uppercase tracking-[0.1em] text-orange hover:underline"
        >
          <span className="lamp text-orange" aria-hidden />
          The agent is waiting on an answer — jump to it
        </button>
      )}

      {blocked && (
        <p
          className="mt-2 px-1 text-xs leading-relaxed text-dim"
          data-testid="composer-blocked-hint"
        >
          {blockedReason}
        </p>
      )}
    </div>
  );
}
