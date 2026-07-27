import { ArrowUp, HelpCircle, Loader2 } from "lucide-react";
import { createContext, useContext, useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { askInputOf, composeAnswer, pickedChoices } from "../lib/askUser";
import { cn } from "../lib/cn";

export interface AskAnswerApi {
  /** Tool call id of the ask awaiting an answer, or null when none is pending. */
  pendingId: string | null;
  /** An answer is in flight — the turn it resumes has not reported back yet. */
  submitting: boolean;
  submit: (text: string) => void;
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
  const api = useAskAnswer();
  const input = askInputOf(call.input);
  // Duplicate labels would make a multi-select join ambiguous.
  const choices = [...new Set(input.choices ?? [])];
  const multiple = input.multiple === true;
  const pending = api != null && api.pendingId === call.id;
  const answer = call.output;

  const [selected, setSelected] = useState<string[]>([]);
  const [text, setText] = useState("");

  const toggle = (c: string) =>
    setSelected((prev) => {
      if (prev.includes(c)) return prev.filter((x) => x !== c);
      return multiple ? [...prev, c] : [c];
    });

  const picked = answer !== undefined ? pickedChoices(answer, choices) : null;
  const canSend =
    pending && !api.submitting && (selected.length > 0 || text.trim().length > 0);

  const send = () => {
    if (!canSend) return;
    api.submit(composeAnswer(selected, text));
  };

  return (
    <div
      data-testid="ask-user-card"
      data-pending={pending}
      className="rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-text"
    >
      <div className="flex items-start gap-2">
        <HelpCircle size={16} className="mt-0.5 shrink-0 text-warning" />
        <div className="min-w-0 flex-1">
          <span className="font-medium text-warning">Asked: </span>
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
                    "chip text-left",
                    pending && "cursor-pointer hover:border-warning",
                    (pending ? selected.includes(c) : picked?.has(c)) &&
                      "border-warning bg-warning/15 font-medium",
                  )}
                >
                  {c}
                </button>
              ))}
            </div>
          )}

          {pending && (
            <div className="mt-2 flex items-end gap-2">
              <input
                data-testid="ask-user-text"
                value={text}
                onChange={(e) => setText(e.target.value)}
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
                className="min-w-0 flex-1 rounded-[var(--radius)] border bg-transparent px-2 py-1 text-sm outline-none placeholder:text-faint focus:border-accent disabled:opacity-60"
              />
              <button
                type="button"
                data-testid="ask-user-send"
                onClick={send}
                disabled={!canSend}
                aria-label="Send answer"
                className="btn-primary shrink-0 !px-2.5 !py-1"
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
              data-testid="ask-user-answer"
              className="mt-1.5 whitespace-pre-wrap text-muted"
            >
              {answer}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
