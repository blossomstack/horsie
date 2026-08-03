import { CircleAlert, Square, Trash2 } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import { ApiRequestError, MAIN_AGENT } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { AskAnswerProvider } from "../components/AskUserCard";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { ContextStatsPanel } from "../components/ContextStatsPanel";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { SettingsMenu } from "../components/SettingsMenu";
import { StatusBadge } from "../components/StatusBadge";
import { TaskListPanel } from "../components/TaskListPanel";
import { Transcript } from "../components/Transcript";
import { useSessionStream } from "../hooks/useSessionStream";
import { useUiSettings } from "../hooks/useUiSettings";
import {
  useDeleteSession,
  useAnswerAsks,
  useSendMessage,
  useSession,
  useAgent,
  useStopSession,
} from "../hooks/useSessions";
import { sessionTitle } from "../lib/format";
import { statusMeta } from "../lib/status";

/** Friendly label for a resource-preparation progression stage. Unknown stages
 * fall back to a de-slugged form so a new backend stage still reads sensibly. */
function progressionLabel(stage: string): string {
  const known: Record<string, string> = {
    provisioning_runtime: "Starting runtime…",
    scanning_workspace: "Scanning workspace…",
    connecting_tools: "Connecting tools…",
    ready: "Ready",
  };
  return known[stage] ?? `${stage.replace(/_/g, " ")}…`;
}

/** Router state carrying the first message through the navigation from a new
 * chat draft — see `NewSessionView`. */
export interface PendingFirstMessageState {
  pendingFirstMessage: string;
}

export function SessionView() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const location = useLocation();
  const { data: detail, isLoading } = useSession(id);
  const {
    stream,
    addOptimisticUser,
    removeOptimisticUser,
    ackOptimisticUser,
    loadMore,
  } = useSessionStream(id);
  const { data: mainAgent } = useAgent(id, MAIN_AGENT);
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
  const sentPendingRef = useRef(false);

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

  const focusPendingAsk = () => {
    document
      .querySelector('[data-testid="ask-user-card"][data-pending="true"]')
      ?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  // A new chat's first message is sent here, once this view's own session
  // fetch has resolved — not from NewSessionView immediately after create.
  // Two reasons: it gives the server's async provisioning the same
  // wall-clock slack a full page mount gives it under any load, local or CI;
  // and it guarantees `qk.session(id)`'s query cache already exists before
  // `useSendMessage`'s optimistic title update runs, since that update is a
  // no-op against a not-yet-populated cache entry (same guard pattern as
  // `applyGlobalEvent`).
  useEffect(() => {
    if (!id || isLoading || sentPendingRef.current) return;
    const pending = (location.state as PendingFirstMessageState | null)
      ?.pendingFirstMessage;
    if (!pending) return;
    sentPendingRef.current = true;
    handleSend(id, pending);
    navigate(location.pathname, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, isLoading]);

  // `null` is a real answer, not a missing one: a session the server has not
  // loaded since it started has no status to report, and guessing `Idle` would
  // dress that up as knowledge.
  const status = stream.liveStatus ?? detail?.status ?? null;
  const terminal =
    status === SessionStatusKind.Unrecoverable
      ? (stream.statusReason ?? detail?.lastError ?? "This session cannot run again.")
      : null;
  const totalTokens = stream.usage.input + stream.usage.output;

  // The server names what is answerable: live from the status frame, and from
  // the session detail for a page opened on an already-parked session. Nothing
  // is inferred from the transcript, so a question stays answerable whether or
  // not the session happens to be loaded.
  const pendingAsks = stream.livePendingAsks ?? detail?.pendingAsks ?? [];
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
  }, [stream.messages, stream.streaming, stream.orphanTools.length]);

  // Reset scroll intent when switching sessions.
  useEffect(() => {
    stick.current = true;
    setSendError(null);
  }, [id]);

  if (!id) return null;

  const handleStop = async () => {
    try {
      await stop.mutateAsync(id);
    } catch {
      /* surfaced via status */
    }
  };

  const handleDelete = async () => {
    if (!confirm("Delete this session? This cannot be undone.")) return;
    try {
      await del.mutateAsync(id);
      navigate("/");
    } catch {
      /* ignore */
    }
  };

  const title = sessionTitle(detail?.name);
  // The composer grows its own Stop button while a turn runs; the header one is
  // for stopping a turn you have scrolled away from.
  const stoppable = status === SessionStatusKind.Running;

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
        <div className="flex h-full min-w-0 flex-1 flex-col">
          {/* The header strip: what this channel is, what it is doing, and
              every setting it was launched with — read at a glance, never
              clicked. */}
          <header className="border-b bg-panel px-4 pb-2.5 pt-3 sm:px-6">
            <div className="flex items-center gap-2 sm:gap-3">
              <RailToggle />
              <h1
                data-testid="session-title"
                className="min-w-0 flex-1 truncate text-[15px] font-semibold tracking-tight text-legend"
              >
                {title}
              </h1>
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
                  <span className="legend text-current">Reconnecting</span>
                </span>
              )}
              <div className="flex items-center gap-0.5">
                <SettingsMenu />
                {stoppable && (
                  <button
                    className="key key-stop !px-2.5"
                    onClick={handleStop}
                    disabled={stop.isPending}
                    title="Stop this turn (queued messages are kept)"
                    data-testid="session-stop"
                  >
                    <Square size={11} className="fill-current" aria-hidden />
                    Stop
                  </button>
                )}
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
              </div>
            </div>

            <div className="mt-2.5 flex flex-wrap items-center gap-x-5 gap-y-2">
              <ContextStatsPanel
                agent={mainAgent}
                sessionTotal={detail?.usageTotal}
                totalTokens={totalTokens}
              />
              {detail && <SessionConfigBar mode="locked" detail={detail} />}
            </div>
          </header>

          {/* Transcript */}
          <div
            ref={scrollRef}
            onScroll={onScroll}
            data-testid="transcript-scroll"
            className="flex-1 overflow-y-auto"
          >
            {isLoading && stream.messages.length === 0 ? (
              <div className="flex h-full items-center justify-center gap-2">
                <span className="lamp lamp-live text-amber-ink" aria-hidden />
                <span className="legend">Loading transcript</span>
              </div>
            ) : stream.messages.length === 0 &&
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
                  messages={stream.messages}
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
          {stream.progression && (
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
          <Composer
            status={status}
            busy={send.isPending}
            onSend={(text) => handleSend(id, text)}
            onStop={handleStop}
            onFocusAsk={focusPendingAsk}
            askPending={answerableIds.length > 0}
          />
        </div>

        <TaskListPanel tasks={stream.tasks} />
      </div>
    </AskAnswerProvider>
  );
}
