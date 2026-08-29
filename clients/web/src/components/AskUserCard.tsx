import { HelpCircle } from "lucide-react";
import { createContext, useContext } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { askInputOf } from "../lib/askUser";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";
import { AskAnswerForm } from "./AskAnswerForm";

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
  const pending = api != null && api.pendingIds.includes(call.id);
  const answer = call.output;
  // A rejected or abandoned ask has an error result rather than an answer:
  // it was never put to the user, and it can never be answered now.
  const superseded = answer !== undefined && call.isError === true;
  const many = api != null && api.pendingIds.length > 1;

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

          <AskAnswerForm
            choices={choices}
            multiple={input.multiple === true}
            answering={pending}
            answer={answer}
            submitting={api?.submitting === true}
            canSend={pending && !api.submitting && api.canSubmit}
            onChange={(text) => api?.setAnswer(call.id, text)}
            onSend={() => api?.submit()}
            sendLabel={
              many ? t("askUser.sendAllAnswers") : t("askUser.sendAnswer")
            }
            note={
              pending && many ? (
                <p className="mt-1 text-xs text-faint">
                  {t("askUser.oneOfMany", { total: api.pendingIds.length })}
                </p>
              ) : undefined
            }
          />

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
