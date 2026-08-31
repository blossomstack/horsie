import { Bell, HelpCircle, Trash2 } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { InboxScope } from "../../api/client";
import { InboxState, type InboxMessageView } from "../../api/types";
import { InboxMessage } from "../../components/InboxMessage";
import { ListDetail, NothingSelected } from "../../components/ListDetail";
import { useDeleteInboxMessages, useInbox, useMarkInboxRead } from "../../hooks/useInbox";
import { useSessionList } from "../../hooks/useSessions";
import { askConfirm } from "../../lib/confirm";
import { cn } from "../../lib/cn";
import { relativeTime, sessionTitle } from "../../lib/format";

/** The three ways to read the inbox, and the slice each one asks the server
 * for. `open` is both kinds — a notice is open until it is replied to — so the
 * view that says "needs answer" narrows it to the asks, which are the only
 * messages holding an agent still. */
const VIEWS = {
  all: "all",
  unread: "unread",
  answer: "open",
} as const satisfies Record<string, InboxScope>;

type View = keyof typeof VIEWS;

const VIEW_LABELS = {
  all: "inbox.filterAll",
  unread: "inbox.filterUnread",
  answer: "inbox.filterOpen",
} as const;

export function InboxPage() {
  const { t } = useTranslation();
  const [view, setView] = useState<View>("all");
  const [picked, setPicked] = useState<ReadonlySet<string>>(new Set());
  // The message being read, kept rather than looked up by id alone: opening an
  // unread one marks it read, which takes it straight out of the unread slice
  // — and a pane that empties itself the moment you start reading is worse
  // than a row that lingers in a list.
  const [opened, setOpened] = useState<InboxMessageView | null>(null);
  const { data, isLoading, isError } = useInbox(VIEWS[view]);
  const { data: sessions } = useSessionList();
  const markRead = useMarkInboxRead();
  const del = useDeleteInboxMessages();

  const messages = (data?.messages ?? []).filter(
    (m) => view !== "answer" || m.body.kind === "Ask",
  );
  const current = opened
    ? (data?.messages.find((m) => m.id === opened.id) ?? opened)
    : null;

  /** The session a message came from, by name.
   *
   * Resolved here because the server deliberately stores no copy of the name:
   * a snapshot would file a renamed session under whatever it used to be
   * called. A short id when the session is gone — nothing else is true. */
  const sessionName = (id: string) => {
    const found = sessions?.find((s) => s.id === id);
    return found ? sessionTitle(found.name) : id.slice(0, 8);
  };

  const show = (m: InboxMessageView) => {
    setOpened(m);
    if (m.readAt === undefined) markRead.mutate([m.id]);
  };

  const toggle = (id: string) =>
    setPicked((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  // A selection that survived a filter change is a selection nobody can see,
  // and the delete confirm can only name what open asks are in the list it
  // is looking at.
  const changeView = (next: View) => {
    setView(next);
    setPicked(new Set());
  };

  const remove = async () => {
    const ids = [...picked];
    const asks = messages.filter(
      (m) =>
        picked.has(m.id) && m.body.kind === "Ask" && m.state === InboxState.Open,
    ).length;
    // One selected message that *is* the question gets its own sentence: the
    // general form counts asks separately from messages, which at one of each
    // reads as "delete this message? 1 of them is a question".
    const prompt =
      ids.length === 1 && asks === 1
        ? t("inbox.confirmDeleteOnlyAsk")
        : asks > 0
          ? `${t("inbox.confirmDelete", { count: ids.length })} ${t("inbox.declineWarning", { count: asks })}`
          : t("inbox.confirmDelete", { count: ids.length });
    if (!(await askConfirm(prompt))
    )
      return;
    del.mutate(ids);
    setPicked(new Set());
    if (opened && picked.has(opened.id)) setOpened(null);
  };

  return (
    <ListDetail
      testId="inbox-page"
      title={t("inbox.title")}
      action={
        picked.size > 0 && (
          <button
            className="key key-stop key-sm shrink-0"
            onClick={remove}
            data-testid="inbox-delete-selected"
          >
            <Trash2 size={14} aria-hidden />
            {t("inbox.deleteSelected", { count: picked.size })}
          </button>
        )
      }
      filters={
        <div className="flex items-center gap-1.5 px-3 pb-2">
          {(Object.keys(VIEWS) as View[]).map((v) => (
            <button
              key={v}
              type="button"
              className="chip chip-toggle"
              data-selected={view === v}
              data-testid={`inbox-filter-${v}`}
              onClick={() => changeView(v)}
            >
              {t(VIEW_LABELS[v])}
            </button>
          ))}
        </div>
      }
      detail={
        current ? (
          <InboxMessage
            key={current.id}
            message={current}
            sessionName={sessionName(current.sessionId)}
          />
        ) : (
          <NothingSelected>{t("inbox.pickOne")}</NothingSelected>
        )
      }
    >
      <div className="space-y-px">
          {isLoading && (
            <p className="empty">{t("common.loading")}</p>
          )}
          {isError && (
            <p className="px-2.5 py-6 text-sm text-red-ink">
              {t("common.unreachableShort")}
            </p>
          )}
          {data && messages.length === 0 && (
            <p className="empty" data-testid="inbox-empty">
              {view === "all" ? t("inbox.empty") : t("inbox.noneInView")}
            </p>
          )}
          {messages.map((m) => {
            const ask = m.body.kind === "Ask";
            const waiting = ask && m.state === InboxState.Open;
            return (
              <div
                key={m.id}
                className="row px-2 py-1.5"
                data-testid="inbox-row"
                data-message-id={m.id}
                aria-selected={current?.id === m.id}
              >
                <input
                  type="checkbox"
                  checked={picked.has(m.id)}
                  onChange={() => toggle(m.id)}
                  aria-label={t("inbox.select", { title: m.title })}
                  data-testid={`inbox-select-${m.id}`}
                />
                <button
                  type="button"
                  className="min-w-0 flex-1 text-left"
                  onClick={() => show(m)}
                  data-testid="inbox-open"
                >
                  <span className="flex items-center gap-1.5">
                    {ask ? (
                      <HelpCircle
                        size={14}
                        aria-hidden
                        className={cn(
                          "shrink-0",
                          // Only an unanswered question is an agent standing
                          // still; an answered one is history, and colouring
                          // both would spend the loud tone on the wrong half.
                          waiting ? "text-live-ink" : "text-faint",
                        )}
                      />
                    ) : (
                      <Bell size={14} aria-hidden className="shrink-0 text-faint" />
                    )}
                    <span className="item-title truncate">{m.title}</span>
                    {m.readAt === undefined && (
                      <span
                        data-testid="inbox-unread-dot"
                        aria-label={t("inbox.unread")}
                        className="ml-auto h-1.5 w-1.5 shrink-0 rounded-full bg-accent"
                      />
                    )}
                  </span>
                  <span className="legend mt-0.5 block truncate">
                    {sessionName(m.sessionId)} · {relativeTime(m.createdAt)}
                  </span>
                </button>
              </div>
            );
          })}
      </div>
    </ListDetail>
  );
}
