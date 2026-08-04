import { ArrowUp, Square } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { SessionStatusKind } from "../api/types";
import { statusMeta } from "../lib/status";

/**
 * The composer: one field, one button, riding inside it.
 *
 * The button is icon-only. The rule it breaks — "an unlabelled icon is a
 * control you have to learn" — is retired deliberately here and nowhere else:
 * these are the two most-pressed controls in the product, they never move, and
 * an upward arrow beside a text field is not a symbol anyone has to be taught.
 * Both carry their word in `title` and `aria-label`.
 *
 * While a turn runs the button is Stop and only Stop. Queueing the next
 * message is still supported and still useful — Enter does it, and the
 * placeholder says so — but a turn in flight has exactly one thing worth
 * pressing.
 */
export function Composer({
  status,
  busy,
  blockedReason = null,
  onSend,
  onStop,
}: {
  status: SessionStatusKind | null | undefined;
  busy: boolean;
  blockedReason?: string | null;
  onSend: (text: string) => void;
  onStop: () => void;
}) {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);
  const meta = statusMeta(status);
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;
  const blocked = blockedReason != null;

  // Auto-grow the textarea up to a cap. The fractional line box
  // (line-height 1.625 × 15px) leaves sub-pixel overflow below the cap, which
  // browsers that paint scrollbars (Safari zoomed, classic-scrollbar setups)
  // render as a permanent scrollbar on an empty input — so hide overflow
  // until the cap, where the content genuinely overflows and `auto` lets the
  // user scroll.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    const capped = el.scrollHeight > 200;
    el.style.overflowY = capped ? "auto" : "hidden";
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
      {/* The focus ring rides the whole panel, and the textarea inside it is
          `outline-none` on purpose: the field fills the panel, so its own
          2px offset outline had nowhere to go but the sliver below it, and
          the only segment `overflow-hidden` did not clip read as a solid
          coloured line ruled under the input. One control, one ring. */}
      <div className="panel relative overflow-hidden transition-shadow focus-within:border-amber focus-within:shadow-[0_0_0_3px_var(--focus-ring)]">
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
          // `pr-14` reserves the button's lane so a long line never runs
          // underneath it.
          className="max-h-[200px] w-full resize-none bg-transparent py-3 pl-3.5 pr-14 text-[0.9375rem] leading-relaxed text-legend outline-none placeholder:text-faint disabled:opacity-60"
        />

        <div className="absolute bottom-2 right-2">
          {running ? (
            <button
              className="key key-stop !h-8 !w-8 !p-0"
              onClick={onStop}
              disabled={busy}
              title="Stop this turn — queued messages are kept"
              aria-label="Stop this turn"
              data-testid="composer-stop"
            >
              <Square size={12} className="fill-current" aria-hidden />
            </button>
          ) : (
            <button
              className="key key-go !h-8 !w-8 !p-0"
              onClick={submit}
              disabled={!text.trim() || !meta.canSend || busy || blocked}
              title={blockedReason ?? "Send — Enter sends, Shift+Enter starts a new line"}
              aria-label="Send message"
              data-testid="composer-send"
            >
              <ArrowUp size={15} aria-hidden />
            </button>
          )}
        </div>
      </div>

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
