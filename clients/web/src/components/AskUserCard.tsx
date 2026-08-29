import { ArrowUp, HelpCircle, Loader2 } from "lucide-react";
import { createContext, useContext, useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { askInputOf, composeAnswer, pickedChoices } from "../lib/askUser";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";

export interface AskAnswerApi {
  /** Tool call ids of every ask awaiting an answer. A turn may ask more than
   * once, and the run resumes only when all of them have been answered. */
  pendingIds: string[];
  /** An answer is in flight — the turn it resumes has not reported back yet. */
  submitting: boolean;
  /** The answer collected so far for each pending ask, keyed by call id. */
  answers: Record<string, string>;
  setAnswer: (toolCallId: string, text: string) => void;
  /** Every pending ask has an answer, so the set can be sent. */
  canSubmit: boolean;
  /** Send every answer at once. A partial set is refused by the server. */
  submit: () => void;
}

const AskAnswerContext = createContext<AskAnswerApi | null>(null);

export const AskAnswerProvider = AskAnswerContext.Provider;

/** Null outside a session view — every other call site renders read-only, which
 * is the right default for a historical transcript. */
export function useAskAnswer(): AskAnswerApi | null {
  return useContext(AskAnswerContext);
}

/** An `ask_user` call: the question, its suggested answers, and — while it is
 * the pending ask — the controls to answer it. */
export function AskUserCard({ call }: { call: RenderedToolCall }) {
  const { t } = useTranslation();
  const api = useAskAnswer();
  const input = askInputOf(call.input);
  // Duplicate labels would make a multi-select join ambiguous.
  const choices = [...new Set(input.choices ?? [])];
  const multiple = input.multiple === true;
  const pending = api != null && api.pendingIds.includes(call.id);
  const answer = call.output;
  // A rejected or abandoned ask has an error result rather than an answer:
  // it was never put to the user, and it can never be answered now.
  const superseded = answer !== undefined && call.isError === true;

  const [selected, setSelected] = useState<string[]>([]);
  const [text, setText] = useState("");

  // What this card contributes to the turn's answer set. Reported upward on
  // every change, because the send button belongs to the group, not the card.
  const report = (picks: string[], free: string) => {
    api?.setAnswer(call.id, composeAnswer(picks, free));
  };

  const toggle = (c: string) => {
    setSelected((prev) => {
      const next = prev.includes(c)
        ? prev.filter((x) => x !== c)
        : multiple
          ? [...prev, c]
          : [c];
      report(next, text);
      return next;
    });
  };

  const picked = answer !== undefined ? pickedChoices(answer, choices) : null;
  const canSend = pending && !api.submitting && api.canSubmit;

  const send = () => {
    if (!canSend) return;
    api.submit();
  };

  return (
    <div
      data-testid="ask-user-card"
      data-pending={pending}
      className="rounded-[var(--radius-control)] bg-live-quiet px-3 py-2 text-sm text-legend"
    >
      <div className="flex items-start gap-2">
        <HelpCircle size={16} className="mt-0.5 shrink-0 text-live-ink" />
        {/* `break-words` is load-bearing, not defensive: a question or an
            answer routinely carries a path, a URL or an identifier with no
            break opportunity, and without it that single token widens the
            card past the transcript column. */}
        <div className="min-w-0 flex-1 break-words">
          <span className="font-medium text-live-ink">Asked: </span>
          {input.question ?? ""}

          {choices.length > 0 && (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {choices.map((c) => (
                <button
                  key={c}
                  type="button"
                  data-testid="ask-user-choice"
                  data-value={c}
                  data-selected={pending ? selected.includes(c) : picked?.has(c) === true}
                  disabled={!pending || api.submitting}
                  onClick={() => toggle(c)}
                  className={cn(
                    // `.chip` alone sets `white-space: nowrap`, which sent a
                    // long choice label straight out of the card — the model
                    // writes these, so their length is unbounded.
                    "chip chip-wrap chip-toggle",
                    (pending ? selected.includes(c) : picked?.has(c)) &&
                      "font-medium",
                  )}
                >
                  {c}
                </button>
              ))}
            </div>
          )}

          {pending && api.pendingIds.length > 1 && (
            <p className="mt-1 text-xs text-faint">
              One of {api.pendingIds.length} questions — all of them are sent
              together.
            </p>
          )}

          {pending && (
            <div className="mt-2 flex items-end gap-2">
              <input
                data-testid="ask-user-text"
                value={text}
                onChange={(e) => {
                  setText(e.target.value);
                  report(selected, e.target.value);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    send();
                  }
                }}
                disabled={api.submitting}
                placeholder={
                  choices.length > 0 ? "Or answer in your own words…" : "Your answer…"
                }
                // No `outline-none` here, unlike the composer: this input has
                // nothing clipping it and no ring of its own, and it is the
                // field that unblocks a parked agent — the one place to lose
                // the focus ring is not this one.
                className="min-w-0 flex-1 rounded-[var(--radius-control)] bg-transparent px-2 py-1 text-sm placeholder:text-faint focus:border-live disabled:opacity-60"
              />
              <button
                type="button"
                data-testid="ask-user-send"
                onClick={send}
                disabled={!canSend}
                aria-label={
                  api != null && api.pendingIds.length > 1
                    ? "Send all answers"
                    : "Send answer"
                }
                className="key key-go shrink-0 !px-2.5 !py-1"
              >
                {api.submitting ? (
                  <Loader2 size={15} className="animate-spin" />
                ) : (
                  <ArrowUp size={15} />
                )}
              </button>
            </div>
          )}

          {answer !== undefined && (
            <p
              data-testid={superseded ? "ask-user-superseded" : "ask-user-answer"}
              className={cn(
                "mt-1.5 break-words whitespace-pre-wrap",
                superseded ? "text-faint italic" : "text-dim",
              )}
            >
              {superseded ? t("askUser.notAnswered", { answer }) : answer}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
