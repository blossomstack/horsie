import { ArrowUp, Loader2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { composeAnswer, pickedChoices } from "../lib/askUser";
import { cn } from "../lib/cn";

/**
 * The control that answers an `ask_user` question: its suggested choices as
 * toggles, a box for words of your own, and the key that sends.
 *
 * A component rather than markup inside the transcript's card, because the
 * inbox answers the same parked calls from a page of its own. A second copy
 * would be a second chance to disagree with [`composeAnswer`] about what a
 * multi-select answer looks like, and the model reads the difference.
 *
 * The composed answer is reported upward on every change; whether it may be
 * sent is the caller's rule, because a turn that asked twice sends both
 * answers or neither.
 */
export function AskAnswerForm({
  choices,
  multiple,
  answering,
  answer,
  submitting,
  canSend,
  onChange,
  onSend,
  sendLabel,
  note,
}: {
  choices: string[];
  /** Whether several choices may be picked at once. */
  multiple: boolean;
  /** False once the question is settled: the chips become history. */
  answering: boolean;
  /** The answer already given, if there is one — which chips it picked is
   *  recovered from it. */
  answer?: string;
  submitting: boolean;
  canSend: boolean;
  onChange: (answer: string) => void;
  onSend: () => void;
  sendLabel: string;
  /** Between the choices and the box, for whatever the caller has to add. */
  note?: ReactNode;
}) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<string[]>([]);
  const [text, setText] = useState("");

  const report = (picks: string[], free: string) =>
    onChange(composeAnswer(picks, free));

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
  const send = () => {
    if (canSend) onSend();
  };

  return (
    <>
      {choices.length > 0 && (
        <div className="mt-1.5 flex flex-wrap gap-1.5">
          {choices.map((c) => (
            <button
              key={c}
              type="button"
              data-testid="ask-user-choice"
              data-value={c}
              data-selected={
                answering ? selected.includes(c) : picked?.has(c) === true
              }
              disabled={!answering || submitting}
              onClick={() => toggle(c)}
              className={cn(
                // `.chip` alone sets `white-space: nowrap`, which sent a long
                // choice label straight out of the card — the model writes
                // these, so their length is unbounded.
                "chip chip-wrap chip-toggle",
                (answering ? selected.includes(c) : picked?.has(c)) &&
                  "font-medium",
              )}
            >
              {c}
            </button>
          ))}
        </div>
      )}

      {note}

      {answering && (
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
            disabled={submitting}
            placeholder={
              choices.length > 0
                ? t("askUser.orOwnWords")
                : t("askUser.yourAnswer")
            }
            // No `outline-none` here, unlike the composer: this input has
            // nothing clipping it and no ring of its own, and it is the field
            // that unblocks a parked agent — the one place to lose the focus
            // ring is not this one.
            className="min-w-0 flex-1 rounded-[var(--radius-control)] bg-transparent px-2 py-1 text-sm placeholder:text-faint focus:border-live disabled:opacity-60"
          />
          <button
            type="button"
            data-testid="ask-user-send"
            onClick={send}
            disabled={!canSend}
            aria-label={sendLabel}
            className="key key-go shrink-0 !px-2.5 !py-1"
          >
            {submitting ? (
              <Loader2 size={15} className="animate-spin" />
            ) : (
              <ArrowUp size={15} />
            )}
          </button>
        </div>
      )}
    </>
  );
}
