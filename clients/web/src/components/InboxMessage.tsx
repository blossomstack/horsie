import { ArrowUp, ArrowUpRight, Bell, HelpCircle, Loader2 } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { Link } from "react-router-dom";
import { MAIN_AGENT } from "../api/client";
import { InboxState, type InboxMessageView } from "../api/types";
import { useReplyToInboxMessage } from "../hooks/useInbox";
import { relativeTime } from "../lib/format";
import { AskAnswerForm } from "./AskAnswerForm";
import { Prose } from "./Prose";

/** What became of a question, in the words of the person it happened to. */
function outcome(state: InboxState, t: TFunction): string {
  switch (state) {
    case InboxState.Answered:
      return t("inbox.answered");
    case InboxState.Declined:
      return t("inbox.declined");
    default:
      return t("inbox.closed");
  }
}

/**
 * One message, read.
 *
 * Mounted with the message's id as its key, so the half-typed answer to one
 * question never appears under the next.
 */
export function InboxMessage({
  message,
  sessionName,
}: {
  message: InboxMessageView;
  /** Resolved by the page from the session list — the message carries an id. */
  sessionName: string;
}) {
  const { t } = useTranslation();
  const reply = useReplyToInboxMessage();
  const [answer, setAnswer] = useState("");

  const ask = message.body.kind === "Ask" ? message.body.value : null;
  const notice = message.body.kind === "Notice" ? message.body.value : null;
  // A reply that landed settles the question here, whatever the row still
  // says: this page may be reading a slice the answered message has left, and
  // an armed send key on a settled ask only earns a 409.
  const settled = message.state !== InboxState.Open || reply.isSuccess;
  const send = () => reply.mutate({ id: message.id, text: answer });

  // The main agent's page is the session's own path. `/agents/main` reaches
  // the same view, but reads as one agent among several.
  const to =
    message.agentId === MAIN_AGENT
      ? `/sessions/${message.sessionId}`
      : `/sessions/${message.sessionId}/agents/${message.agentId}`;

  return (
    <article
      data-testid="inbox-message"
      className="mx-auto flex max-w-3xl flex-col gap-4 px-6 py-5"
    >
      <header>
        <div className="flex items-start gap-2">
          {ask ? (
            <HelpCircle size={16} className="mt-1 shrink-0 text-live-ink" aria-hidden />
          ) : (
            <Bell size={16} className="mt-1 shrink-0 text-faint" aria-hidden />
          )}
          <h1 className="page-title break-words">{message.title}</h1>
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="legend">
            {ask ? t("inbox.kindAsk") : t("inbox.kindNotice")}
          </span>
          <span className="legend truncate">{sessionName}</span>
          <span className="legend">{relativeTime(message.createdAt)}</span>
          <Link to={to} className="key key-sm" data-testid="inbox-open-session">
            {t("inbox.openSession")}
            <ArrowUpRight size={14} aria-hidden />
          </Link>
        </div>
      </header>

      {ask ? (
        <div className="notice notice-live flex-col items-stretch text-legend">
          {/* The title is the question's first line, so a short one is the
              heading above verbatim and printing it again reads as a stutter. */}
          {ask.question !== message.title && (
            <p className="break-words">{ask.question}</p>
          )}
          <AskAnswerForm
            choices={[...new Set(ask.choices)]}
            multiple={ask.multiple}
            answering={!settled}
            submitting={reply.isPending}
            canSend={answer.trim() !== "" && !reply.isPending}
            onChange={setAnswer}
            onSend={send}
            sendLabel={t("askUser.sendAnswer")}
          />
          {settled && (
            <p data-testid="inbox-outcome" className="mt-1.5 text-dim">
              {reply.isSuccess
                ? t("inbox.answered")
                : outcome(message.state, t)}
            </p>
          )}
        </div>
      ) : (
        notice && (
          <>
            <Prose text={notice.body} />
            {/* A notice parks nothing, so this is an ordinary message to that
                agent — the same one the session's composer would send. */}
            <div className="flex items-end gap-2">
              <textarea
                data-testid="inbox-reply-text"
                className="field min-h-[3.5rem]"
                rows={2}
                value={answer}
                onChange={(e) => setAnswer(e.target.value)}
                placeholder={t("inbox.replyPlaceholder")}
                disabled={reply.isPending}
              />
              <button
                type="button"
                data-testid="inbox-reply-send"
                className="key key-go shrink-0 !px-2.5 !py-1"
                onClick={send}
                disabled={answer.trim() === "" || reply.isPending}
                aria-label={t("inbox.send")}
              >
                {reply.isPending ? (
                  <Loader2 size={15} className="animate-spin" />
                ) : (
                  <ArrowUp size={15} />
                )}
              </button>
            </div>
          </>
        )
      )}
    </article>
  );
}
