import { CircleAlert, ListTodo, Trash2 } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ApiRequestError, MAIN_AGENT } from "../api/client";
import { SessionStatusKind, TaskStatus } from "../api/types";
import { AskAnswerProvider } from "../components/AskUserCard";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { ContextGauge } from "../components/ContextGauge";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { SettingsMenu } from "../components/SettingsMenu";
import { StatusBadge } from "../components/StatusBadge";
import { TaskListPanel } from "../components/TaskListPanel";
import { Transcript } from "../components/Transcript";
import { WorkflowRunView } from "./workflows/WorkflowRunView";
import { askConfirm } from "../lib/confirm";
import { usePersistentState } from "../hooks/usePersistentState";
import { useSessionStream } from "../hooks/useSessionStream";
import { useEntryCatalog } from "../hooks/useEntryCatalog";
import { useUiSettings } from "../hooks/useUiSettings";
import {
  useDeleteSession,
  useAnswerAsks,
  useRenameSession,
  useSendMessage,
  useSession,
  useAgent,
  useStopSession,
} from "../hooks/useSessions";
import { cn } from "../lib/cn";
import { sessionTitle } from "../lib/format";
import { progressionLabel, showsProgression, statusMeta } from "../lib/status";

/** The session's name, and the only way a person can change it.
 *
 * The agent's title tool was the sole writer: a session the model never titled
 * kept its raw first message as its name indefinitely, and nothing could
 * correct it. Editing in place rather than behind a dialog because the title is
 * live state on an instrument face, and a rename is one word.
 *
 * A step is titled by its run, so it is read-only there. */
function SessionTitle({
  id,
  name,
  editable,
}: {
  id: string;
  name: string | undefined;
  editable: boolean;
}) {
  const rename = useRenameSession();
  const [draft, setDraft] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  if (draft === null) {
    const title = sessionTitle(name);
    if (!editable) {
      return (
        <h1 data-testid="session-title" className="page-title min-w-0 flex-1 truncate">
          {title}
        </h1>
      );
    }
    return (
      <h1 className="min-w-0 flex-1">
        <button
          type="button"
          className="page-title block w-full cursor-text truncate rounded-[var(--radius-control)] px-1 text-left hover:bg-raised"
          onClick={() => {
            setError(null);
            setDraft(name ?? "");
          }}
          title="Rename this session"
          data-testid="session-title"
        >
          {title}
        </button>
      </h1>
    );
  }

  const commit = async () => {
    const next = draft.trim();
    if (!next || next === (name ?? "")) {
      setDraft(null);
      return;
    }
    try {
      await rename.mutateAsync({ id, name: next });
      setDraft(null);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Rename failed.");
    }
  };

  return (
    <div className="min-w-0 flex-1">
      <input
        className="field page-title w-full !py-0.5"
        value={draft}
        autoFocus
        maxLength={60}
        aria-label="Session name"
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => void commit()}
        onKeyDown={(e) => {
          if (e.key === "Enter") void commit();
          if (e.key === "Escape") {
            setDraft(null);
            setError(null);
          }
        }}
        data-testid="session-title-input"
      />
      {error && (
        <p className="text-xs text-red-ink" data-testid="session-title-error">
          {error}
        </p>
      )}
    </div>
  );
}

/** A session id the server will not serve.
 *
 * Rendered instead of the session chrome, which otherwise reported the failed
 * read as a brand-new session: `sessionTitle(undefined)` is "New session", an
 * unknown status is sendable by design, and the feed lamp is stuck on
 * "Reconnecting" because a 404 fails an `EventSource` for good. Between them
 * they invited you to type into a session that cannot exist.
 *
 * Kept inside the sessions layout, and keeping the rail toggle, for the same
 * reason `NotFoundPage` is: the dead end is the defect, not the copy. */
function SessionUnavailable({ id, error }: { id: string; error: unknown }) {
  const gone = error instanceof ApiRequestError && error.status === 404;
  return (
    <div className="flex h-full flex-col" data-testid="session-unavailable">
      <header className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 data-testid="session-title" className="page-title min-w-0 flex-1 truncate">
          {gone ? "No such session" : "Could not load this session"}
        </h1>
      </header>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          <section className="panel p-4">
            <h2 className="legend">Session id</h2>
            <pre className="screen mt-3 overflow-x-auto px-3 py-2.5 font-mono text-[0.6875rem] leading-relaxed text-legend select-all">
              {id}
            </pre>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              {gone
                ? "It was deleted, or it never existed. Nothing you type here can reach it."
                : error instanceof Error
                  ? error.message
                  : "The read failed."}{" "}
              <Link className="text-legend underline" to="/">
                Your sessions
              </Link>{" "}
              lists the ones that are there.
            </p>
          </section>
        </div>
      </div>
    </div>
  );
}

export function SessionView() {
  const { id, agentId } = useParams<{ id: string; agentId?: string }>();
  const navigate = useNavigate();
  const { data: detail, isLoading, isError, error } = useSession(id);
  // The session's own bundles decide what `/` and `@` offer.
  const entries = useEntryCatalog(detail?.plugins);
  const {
    stream,
    addOptimisticUser,
    removeOptimisticUser,
    ackOptimisticUser,
    loadMore,
  } = useSessionStream(id, agentId ?? MAIN_AGENT);
  const { data: mainAgent } = useAgent(id, agentId ?? MAIN_AGENT);
  const send = useSendMessage();
  const answerAsks = useAnswerAsks();
  const stop = useStopSession();
  const del = useDeleteSession();
  const { values: uiSettings } = useUiSettings();
  const [sendError, setSendError] = useState<string | null>(null);

  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  // When a scroll-back page is loading, holds the scroll height captured just
  // before the prepend so we can restore the viewport position after it lands.
  const loadAnchor = useRef<number | null>(null);

  const handleSend = async (sessionId: string, text: string) => {
    setSendError(null);
    // Echo the message immediately — a live session's SSE push for this same
    // message can arrive before this request resolves, so the echo must exist
    // *before* the request goes out or the real message beats it and the
    // echo is left stuck as an unmatched duplicate.
    const optimisticId = addOptimisticUser(text);
    try {
      const ack = await send.mutateAsync({ id: sessionId, text });
      // From here the server owns the message: the echo is handed its
      // server-side id so the queue can take it over without duplicating it.
      ackOptimisticUser(optimisticId, ack.messageId);
    } catch (e) {
      removeOptimisticUser(optimisticId);
      setSendError(
        e instanceof ApiRequestError ? e.message : "Failed to send message.",
      );
      // Rethrown so the composer can restore what was typed. Swallowing it
      // here is what let an offline send clear the box and lose the message.
      throw e;
    }
  };

  // An answer to a pending ask. Deliberately without an optimistic echo: an
  // answer is persisted as a tool result, never as a user message, so an echo
  // would linger unreconciled forever (and vanish on reload). The card renders
  // the durable answer instead.
  // Answers are all-or-nothing: the run cannot resume while any parked call is
  // still missing a result, so every card's answer goes in one request.
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const submitAnswers = async () => {
    if (!id) return;
    setSendError(null);
    try {
      await answerAsks.mutateAsync({
        id,
        // Whoever asked is whoever this page is scoped to — a workflow step as
        // readily as the main agent. A run has no main agent, so leaving this
        // out is what made answering a parked step a no-op.
        agentId: agentId ?? MAIN_AGENT,
        answers: pendingAsks
          .filter((a) => a.toolCallId)
          .map((a) => ({ toolCallId: a.toolCallId as string, text: answers[a.toolCallId as string] ?? "" })),
      });
      setAnswers({});
    } catch (e) {
      setSendError(
        e instanceof ApiRequestError ? e.message : "Failed to send your answer.",
      );
    }
  };

  // `null` is a real answer, not a missing one: a session the server has not
  // loaded since it started has no status to report, and guessing `Idle` would
  // dress that up as knowledge.
  const status = stream.liveStatus ?? detail?.status ?? null;
  const terminal =
    status === SessionStatusKind.Unrecoverable
      ? (stream.statusReason ?? detail?.lastError ?? "This session cannot run again.")
      : null;

  // The plan panel is available on every session, with or without a list, and
  // its visibility is the operator's standing choice rather than a
  // consequence of whether the agent happened to use the tool.
  const [tasksOpen, setTasksOpen] = usePersistentState("horsie-tasks-open", false);
  const tasks = stream.tasks;
  const tasksDone = tasks.filter((t) => t.status === TaskStatus.Completed).length;

  // Folded from the agent's own log, which is where the agent that asked
  // records its questions — the same connection the transcript arrives on, so
  // there is no second source that could disagree about what is answerable.
  const pendingAsks = stream.livePendingAsks ?? [];
  const answerableIds = useMemo(
    () => pendingAsks.map((a) => a.toolCallId).filter((x): x is string => !!x),
    [pendingAsks],
  );
  const canSubmitAnswers =
    answerableIds.length > 0 &&
    answerableIds.every((id) => (answers[id] ?? "").trim().length > 0);

  // Stick-to-bottom auto scroll; also trigger scroll-back near the top.
  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 96;
    if (el.scrollTop < 80 && stream.hasMoreBefore && !stream.loadingMore) {
      loadAnchor.current = el.scrollHeight;
      loadMore();
    }
  };
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // A just-completed scroll-back prepend: keep the viewport where it was by
    // pushing down by exactly the height the older messages added.
    if (loadAnchor.current != null) {
      el.scrollTop += el.scrollHeight - loadAnchor.current;
      loadAnchor.current = null;
      return;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  }, [stream.items, stream.streaming, stream.orphanTools.length]);

  // Reset scroll intent when switching sessions.
  useEffect(() => {
    stick.current = true;
    setSendError(null);
  }, [id]);

  if (!id) return null;
  // Only on failure, not on `isLoading`: the chrome is drawn while the read is
  // in flight on purpose, because the transcript arrives over its own feed and
  // a spinner in front of it would delay what is already on screen.
  if (isError) return <SessionUnavailable id={id} error={error} />;

  const handleStop = async () => {
    try {
      await stop.mutateAsync(id);
    } catch {
      /* surfaced via status */
    }
  };

  const handleDelete = async () => {
    if (!(await askConfirm("Delete this session? This cannot be undone.")))
      return;
    try {
      await del.mutateAsync(id);
      navigate("/");
    } catch {
      /* ignore */
    }
  };

  // Stop lives only in the composer. The header used to carry a second one
  // "for stopping a turn you have scrolled away from", which was never a real
  // case: the composer is pinned to the bottom of the pane and never scrolls.

  // A run has no single conversation, so the graph is its page. Opening one of
  // its steps routes back here with an agent id, and falls through to the
  // transcript below — the same view, scoped to that agent.
  if (id && detail?.workflow && !agentId) {
    return (
      <WorkflowRunView
        sessionId={id}
        onStop={handleStop}
        onDelete={handleDelete}
      />
    );
  }

  return (
    <AskAnswerProvider
      value={{
        pendingIds: answerableIds,
        submitting: answerAsks.isPending,
        answers,
        setAnswer: (callId, text) =>
          setAnswers((prev) => ({ ...prev, [callId]: text })),
        canSubmit: canSubmitAnswers,
        submit: submitAnswers,
      }}
    >
      <div className="relative flex h-full">
        {/* The column a config menu has to stay inside. Overflow cannot say
            this — nothing between here and the app shell clips — so the
            boundary is declared. */}
        <div
          className="flex h-full min-w-0 flex-1 flex-col"
          data-popover-boundary
        >
          {/* One row, at the same 3.25rem as the rail and task-panel headers,
              so the three columns read as one instrument face. Only live state
              earns a place here: what this is, what it is doing, and how full
              its context is. Settled facts sit behind the info key. */}
          <header className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6">
            <RailToggle />
            <SessionTitle id={id} name={detail?.name} editable={!agentId} />
            <StatusBadge status={status} />
            {/* Durability is the product's whole differentiator, so a dropped
                feed is a first-class state on the panel — not a transcript
                that quietly stops moving while the lamp still says Running. */}
            {!stream.connected && (
              <span
                className="flex shrink-0 items-center gap-2 text-amber-ink"
                data-testid="session-reconnecting"
                title="Lost the live feed. The run continues on the server; this reconnects and replays anything missed."
              >
                <span className="lamp lamp-live" aria-hidden />
                <span className="legend hidden text-current sm:inline">
                  Reconnecting
                </span>
              </span>
            )}
            <div className="flex shrink-0 items-center gap-0.5">
              <ContextGauge
                agent={mainAgent}
                sessionTotal={detail?.usageTotal}
              />
              {/* The plan is always reachable, so a session with no list
                  still has somewhere for one to appear. That there IS a plan
                  is the control's own colour, not a badge stuck on it — a
                  two-digit fraction in the corner of a 2rem key stretched
                  outside the key and pushed the row out of line with the two
                  column headers beside it. The count is in the tooltip, and
                  the panel one click away has it in full. */}
              <button
                className={cn(
                  "key-icon",
                  // Three states have to stay apart, and `bg-raised` is
                  // already what hover paints: a plan exists (ring + full
                  // ink), the panel is open (filled), hovered (filled).
                  tasks.length > 0 &&
                    "!text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]",
                  tasksOpen && "bg-raised !text-legend",
                )}
                onClick={() => setTasksOpen(!tasksOpen)}
                aria-pressed={tasksOpen}
                title={
                  tasks.length
                    ? `${tasksOpen ? "Hide" : "Show"} the plan — ${tasksDone}/${tasks.length} done`
                    : `${tasksOpen ? "Hide" : "Show"} the plan`
                }
                aria-label={
                  tasks.length
                    ? `Toggle the agent's plan — ${tasksDone} of ${tasks.length} done`
                    : "Toggle the agent's plan"
                }
                data-testid="task-list-toggle"
                data-has-plan={tasks.length > 0 ? "true" : undefined}
              >
                <ListTodo size={15} aria-hidden />
              </button>
              <SettingsMenu />
              {/* A step is not deletable on its own: the run is the unit, and
                  its page carries the control. */}
              {!agentId && (
                <button
                  className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
                  onClick={handleDelete}
                  disabled={del.isPending}
                  title="Delete session"
                  aria-label="Delete session"
                  data-testid="session-delete"
                >
                  <Trash2 size={15} aria-hidden />
                </button>
              )}
            </div>
          </header>

          {/* Transcript */}
          <div
            ref={scrollRef}
            onScroll={onScroll}
            data-testid="transcript-scroll"
            className="flex-1 overflow-y-auto"
          >
            {isLoading && stream.items.length === 0 ? (
              <div className="flex h-full items-center justify-center gap-2">
                <span className="lamp lamp-live text-amber-ink" aria-hidden />
                <span className="legend">Loading transcript</span>
              </div>
            ) : stream.items.length === 0 &&
              stream.streaming.length === 0 &&
              status !== SessionStatusKind.Running ? (
              // No status badge here: the header strip carries the session's
              // one status readout, two inches above. A second copy is both a
              // duplicate source of truth and a duplicate `status-badge`
              // testid, which trips Playwright's strict mode whenever an
              // assertion lands while the transcript is still empty.
              <div className="flex h-full flex-col items-center justify-center gap-2.5 px-6 text-center">
                <p className="max-w-sm text-sm leading-relaxed text-dim">
                  {statusMeta(status).hint}
                </p>
                {(stream.statusReason ?? detail?.lastError) && (
                  <p className="max-w-md text-xs leading-relaxed text-red-ink">
                    {stream.statusReason ?? detail?.lastError}
                  </p>
                )}
              </div>
            ) : (
              <>
                {(stream.loadingMore || stream.hasMoreBefore) && (
                  <div
                    className="flex items-center justify-center gap-2 py-3"
                    data-testid="history-load-more"
                  >
                    {stream.loadingMore ? (
                      <>
                        <span
                          className="lamp lamp-live text-amber-ink"
                          aria-hidden
                        />
                        <span className="legend">Loading earlier messages</span>
                      </>
                    ) : (
                      <span className="legend">
                        Scroll up for earlier messages
                      </span>
                    )}
                  </div>
                )}
                <Transcript
                  items={stream.items}
                  streaming={stream.streaming}
                  orphanTools={stream.orphanTools}
                  showLive={status === SessionStatusKind.Running}
                  showThinking={uiSettings.showThinking}
                />
              </>
            )}
          </div>

          {/* Errors */}
          {(sendError || stream.streamError) && (
            <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
              <div
                data-testid="session-error"
                className="flex items-start gap-2 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              >
                <CircleAlert size={16} className="mt-0.5 shrink-0" />
                <span>{sendError ?? stream.streamError}</span>
              </div>
            </div>
          )}

          {/* Resource-preparation progression (live, while a turn spins up) */}
          {stream.progression && showsProgression(stream.progression.stage) && (
            <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
              <div
                data-testid="session-progression"
                data-stage={stream.progression.stage}
                className="flex items-center gap-2 py-1.5"
              >
                <span className="lamp lamp-live text-amber-ink" aria-hidden />
                <span className="legend">
                  {progressionLabel(stream.progression.stage)}
                  {stream.progression.detail ? ` — ${stream.progression.detail}` : ""}
                </span>
              </div>
            </div>
          )}

          {/* Terminal: the runtime is gone and no message can bring it back. */}
          {terminal && (
            <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
              <div
                data-testid="session-terminal"
                className="flex items-start gap-2 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              >
                <CircleAlert size={16} className="mt-0.5 shrink-0" />
                <div className="min-w-0">
                  <p>This session can no longer run: {terminal}</p>
                  <button
                    type="button"
                    className="key key-flat mt-2 !text-red-ink hover:!bg-red-quiet"
                    onClick={() => navigate("/")}
                    data-testid="session-terminal-new"
                  >
                    Start a new session
                  </button>
                </div>
              </div>
            </div>
          )}

          {/* Composer */}
          {/* The channels this session runs on, in the same place the draft
              row occupied before it existed. Read-only — each key opens its
              value rather than a picker. */}
          {detail && <SessionConfigBar mode="locked" detail={detail} />}
          {/* A workflow step takes no messages — the definition drives it — so
              it gets the stop control without the send one. */}
          {agentId && detail?.workflow ? (
            <div className="flex items-center gap-3 border-t px-4 py-2">
              <span className="text-xs text-faint">
                This is a workflow step. It works from its definition, not from
                messages.
              </span>
              {/* Only while there is something to interrupt. The step's own
                  document says what became of it; offering the control on a
                  step that concluded hours ago was the same lie the badge told
                  beside it. */}
              {mainAgent?.status === "running" && (
                <button
                  className="key key-stop ml-auto !px-2 !py-1 text-xs"
                  onClick={handleStop}
                  data-testid="step-stop"
                >
                  Interrupt
                </button>
              )}
            </div>
          ) : (
            <Composer
              status={status}
              busy={send.isPending}
              entries={entries}
              onSend={(text) => handleSend(id, text)}
              onStop={handleStop}
            />
          )}
        </div>

        {tasksOpen && (
          <TaskListPanel tasks={tasks} onClose={() => setTasksOpen(false)} />
        )}
      </div>
    </AskAnswerProvider>
  );
}
