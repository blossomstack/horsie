import {
  CircleAlert,
  Cpu,
  FolderGit2,
  Gauge,
  Loader2,
  Server,
  Square,
  Trash2,
} from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { Composer } from "../components/Composer";
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
  useStopSession,
} from "../hooks/useSessions";
import { basename, compactNumber, sessionTitle } from "../lib/format";
import { statusMeta } from "../lib/status";

function Chip({
  icon,
  children,
  title,
}: {
  icon: ReactNode;
  children: ReactNode;
  title?: string;
}) {
  return (
    <span className="chip" title={title}>
      {icon}
      {children}
    </span>
  );
}

export function SessionView() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { data: detail, isLoading } = useSession(id);
  const { stream, addOptimisticUser, removeOptimisticUser, loadMore } =
    useSessionStream(id);
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

  const status = stream.liveStatus ?? detail?.status ?? SessionStatusKind.Idle;
  const pendingQuestion = stream.pendingQuestion ?? detail?.pendingQuestion ?? null;
  const totalTokens = stream.usage.input + stream.usage.output;

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

  const handleSend = async (text: string) => {
    setSendError(null);
    // Echo the message immediately — a live session's SSE push for this same
    // message can arrive before this request resolves, so the echo must exist
    // *before* the request goes out or the real message beats it and the
    // echo is left stuck as an unmatched duplicate.
    const optimisticId = addOptimisticUser(text);
    try {
      await send.mutateAsync({ id, text });
    } catch (e) {
      removeOptimisticUser(optimisticId);
      setSendError(
        e instanceof ApiRequestError ? e.message : "Failed to send message.",
      );
    }
  };

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
              {detail?.model && (
                <Chip icon={<Cpu size={12} />} title="Model">
                  {detail.model}
                </Chip>
              )}
              {detail?.vendor && (
                <Chip icon={<Server size={12} />} title="Runtime vendor">
                  {detail.vendor}
                </Chip>
              )}
              {detail?.repos?.map((r) => (
                <Chip key={r} icon={<FolderGit2 size={12} />} title={r}>
                  {basename(r)}
                </Chip>
              ))}
              {totalTokens > 0 && (
                <Chip
                  icon={<Gauge size={12} />}
                  title={`${stream.usage.input} in · ${stream.usage.output} out`}
                >
                  {compactNumber(totalTokens)} tok
                </Chip>
              )}
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

        {/* Composer */}
        <Composer
          status={status}
          pendingQuestion={pendingQuestion}
          busy={send.isPending}
          onSend={handleSend}
          onStop={handleStop}
        />
      </div>

      <TaskListPanel tasks={stream.tasks} />
    </div>
  );
}
