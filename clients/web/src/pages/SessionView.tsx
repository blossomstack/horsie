import { CircleAlert, Loader2, Square, Trash2 } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { AskAnswerProvider } from "../components/AskUserCard";
import { Composer } from "../components/Composer";
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
  useSendMessage,
  useSession,
  useSessionUsage,
  useStopSession,
} from "../hooks/useSessions";
import { findPendingAsk } from "../lib/askUser";
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
  const { stream, addOptimisticUser, removeOptimisticUser, loadMore } =
    useSessionStream(id);
  const { data: usageStats } = useSessionUsage(id);
  const send = useSendMessage();
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
      await send.mutateAsync({ id: sessionId, text });
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
  const handleAnswer = async (text: string) => {
    if (!id) return;
    setSendError(null);
    try {
      await send.mutateAsync({ id, text });
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

  const status = stream.liveStatus ?? detail?.status ?? SessionStatusKind.Idle;
  const totalTokens = stream.usage.input + stream.usage.output;

  const pendingAskId = useMemo(
    () =>
      status === SessionStatusKind.AwaitingInput
        ? findPendingAsk(stream.messages)
        : null,
    [status, stream.messages],
  );

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
  const stoppable =
    status !== SessionStatusKind.Stopped &&
    status !== SessionStatusKind.Failed &&
    status !== SessionStatusKind.Provisioning &&
    status !== SessionStatusKind.Running;

  return (
    <AskAnswerProvider
      value={{ pendingId: pendingAskId, submitting: false, submit: handleAnswer }}
    >
      <div className="flex h-full">
        <div className="flex h-full min-w-0 flex-1 flex-col">
          {/* Header */}
          <header
            className="flex items-center gap-3 border-b px-5 py-3"
            style={{ background: "var(--surface)" }}
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2.5">
                <h1 data-testid="session-title" className="truncate text-sm font-semibold text-text">
                  {title}
                </h1>
                <StatusBadge status={status} />
              </div>
              <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                <ContextStatsPanel stats={usageStats} totalTokens={totalTokens} />
              </div>
            </div>

            <div className="ml-auto flex items-center gap-1">
              <SettingsMenu />
              {stoppable && (
                <button
                  className="btn-ghost !px-2.5 text-xs"
                  onClick={handleStop}
                  disabled={stop.isPending}
                  title="Stop the session (preserves the runtime)"
                  data-testid="session-stop"
                >
                  <Square size={14} />
                  Stop
                </button>
              )}
              <button
                className="btn-icon hover:!text-error"
                onClick={handleDelete}
                disabled={del.isPending}
                title="Delete session"
                data-testid="session-delete"
              >
                <Trash2 size={17} />
              </button>
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
              <div className="flex h-full items-center justify-center text-sm text-faint">
                <Loader2 size={18} className="mr-2 animate-spin" />
                Loading transcript…
              </div>
            ) : stream.messages.length === 0 &&
              stream.streaming.length === 0 &&
              status !== SessionStatusKind.Running ? (
              <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center">
                <p className="text-sm font-medium text-muted">
                  {statusMeta(status).hint}
                </p>
                {stream.statusReason ?? detail?.lastError ? (
                  <p className="max-w-md text-xs text-error">
                    {stream.statusReason ?? detail?.lastError}
                  </p>
                ) : (
                  <p className="text-xs text-faint">
                    Send a message below to start the conversation.
                  </p>
                )}
              </div>
            ) : (
              <>
                {(stream.loadingMore || stream.hasMoreBefore) && (
                  <div
                    className="flex items-center justify-center py-2 text-xs text-faint"
                    data-testid="history-load-more"
                  >
                    {stream.loadingMore ? (
                      <>
                        <Loader2 size={12} className="mr-1.5 animate-spin" />
                        Loading earlier messages…
                      </>
                    ) : (
                      <span>Scroll up for earlier messages</span>
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
            <div className="mx-auto w-full max-w-3xl px-4">
              <div
                data-testid="session-error"
                className="flex items-start gap-2 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
              >
                <CircleAlert size={16} className="mt-0.5 shrink-0" />
                <span>{sendError ?? stream.streamError}</span>
              </div>
            </div>
          )}

          {/* Resource-preparation progression (live, while a turn spins up) */}
          {stream.progression && (
            <div className="mx-auto w-full max-w-3xl px-4">
              <div
                data-testid="session-progression"
                data-stage={stream.progression.stage}
                className="flex items-center gap-1.5 py-1 text-xs text-faint"
              >
                <Loader2 size={12} className="animate-spin text-accent" />
                <span>
                  {progressionLabel(stream.progression.stage)}
                  {stream.progression.detail ? ` — ${stream.progression.detail}` : ""}
                </span>
              </div>
            </div>
          )}

          {detail && <SessionConfigBar mode="locked" detail={detail} />}

          {/* Composer */}
          <Composer
            status={status}
            busy={send.isPending}
            askLocked={pendingAskId !== null}
            onSend={(text) => handleSend(id, text)}
            onStop={handleStop}
            onFocusAsk={focusPendingAsk}
          />
        </div>

        <TaskListPanel tasks={stream.tasks} />
      </div>
    </AskAnswerProvider>
  );
}
