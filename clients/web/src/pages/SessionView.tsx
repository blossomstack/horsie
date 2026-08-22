import type { LucideIcon } from "lucide-react";
import {
  ChartNoAxesGantt,
  CircleAlert,
  ListTodo,
  MessageSquareText,
  Trash2,
  Waypoints,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Link, useLocation, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { ApiRequestError, MAIN_AGENT, api } from "../api/client";
import { subSessionReadyToOpen } from "../lib/subSessionTree";
import { SessionStatusKind, TaskStatus } from "../api/types";
import type { SubSessionView } from "../api/types";
import { AskAnswerProvider } from "../components/AskUserCard";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { ContextGauge } from "../components/ContextGauge";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { AgentGraph } from "../components/AgentGraph";
import { SessionPane } from "../components/SessionPane";
import { SessionTimeline } from "../components/SessionTimeline";
import { SettingsMenu } from "../components/SettingsMenu";
import { StatusLamp } from "../components/StatusBadge";
import { TaskListPanel } from "../components/TaskListPanel";
import { Transcript } from "../components/Transcript";
import { TranscriptSpine } from "../components/TranscriptSpine";
import { WorkflowRunView } from "./workflows/WorkflowRunView";
import { askConfirm } from "../lib/confirm";
import { usePersistentState } from "../hooks/usePersistentState";
import { transcriptItems, useSessionStream } from "../hooks/useSessionStream";
import type { TranscriptItem } from "../hooks/useSessionStream";
import { useEntryCatalog } from "../hooks/useEntryCatalog";
import { useUiSettings } from "../hooks/useUiSettings";
import {
  useDeleteSession,
  useDeleteSubSession,
  useAnswerAsks,
  useSendMessage,
  useSession,
  useAgent,
  useStopAgent,
} from "../hooks/useSessions";
import { cn } from "../lib/cn";
import { sessionTitle } from "../lib/format";
import { buildTimeline } from "../lib/timeline";
import { layoutAgentTree } from "../lib/agentTree";
import { progressionLabel, showsProgression, statusMeta } from "../lib/status";

type SessionViewId = "transcript" | "timeline" | "graph";

/** The three views of a session, in the order they read: its prose, its shape
 * in time, its shape in lineage. Icons only — the row is an instrument face,
 * and three labelled keys beside the title would crowd out the title. */
const VIEWS: {
  id: SessionViewId;
  icon: LucideIcon;
  label: string;
  title: string;
  testId: string;
}[] = [
  {
    id: "transcript",
    icon: MessageSquareText,
    label: "Show the transcript",
    title: "Transcript",
    testId: "transcript-toggle",
  },
  {
    id: "timeline",
    icon: ChartNoAxesGantt,
    label: "Show the session timeline",
    title: "Timeline",
    testId: "timeline-toggle",
  },
  {
    id: "graph",
    icon: Waypoints,
    label: "Show the agent graph",
    title: "Agent graph",
    testId: "graph-toggle",
  },
];

/** The session's name.
 *
 * A title, and nothing else. It used to be click-to-edit, which put an
 * editable control where a page title goes and gave the header a hover state
 * that meant "you can type here" on the one line that names what you are
 * looking at. Renaming moved to the session's actions menu in the rail, next
 * to its tags and its delete. */
function SessionTitle({ name }: { name: string | undefined }) {
  return (
    <h1 data-testid="session-title" className="page-title min-w-0 flex-1 truncate">
      {sessionTitle(name)}
    </h1>
  );
}

/** Which conversation this page is, when it is a sub session.
 *
 * Its own component rather than a mode of `SessionTitle`: a sub session is not
 * renamed from here — it names itself, and the session it branched from is the
 * thing with an editable name — so the two share a place in the header and
 * almost none of their behaviour.
 *
 * The session's name stays in front of it, and leads back to it. The page's
 * whole content is one sub session's transcript, and with the rail listing
 * sessions only there is nothing else on screen that says which session this
 * belongs to. */
function SubSessionTitle({
  sessionId,
  sessionName,
  subSession,
}: {
  sessionId: string;
  sessionName: string | undefined;
  subSession: SubSessionView;
}) {
  return (
    <h1
      data-testid="session-title"
      className="page-title flex min-w-0 flex-1 items-baseline gap-1.5 truncate"
    >
      <Link
        to={`/sessions/${sessionId}`}
        className="max-w-[40%] shrink-0 truncate text-dim hover:text-legend"
        title={`Back to ${sessionTitle(sessionName)}`}
      >
        {sessionTitle(sessionName)}
      </Link>
      <span className="shrink-0 text-faint" aria-hidden>
        /
      </span>
      <span className="min-w-0 truncate" data-testid="sub-session-title">
        {subSession.title ?? "untitled sub session"}
      </span>
    </h1>
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
      <header className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 data-testid="session-title" className="page-title min-w-0 flex-1 truncate">
          {gone ? "No such session" : "Could not load this session"}
        </h1>
      </header>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          <section className="section">
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
  const stop = useStopAgent();
  const del = useDeleteSession();
  const delSubSession = useDeleteSubSession();
  /** The sub session this page is showing, when the agent it is scoped to is
   *  one. A subagent and a workflow step are not: they are read here, never
   *  named or deleted here. */
  const openSubSession = agentId
    ? detail?.subSessions?.find((row) => row.id === agentId)
    : undefined;
  const { values: uiSettings } = useUiSettings();
  const [sendError, setSendError] = useState<string | null>(null);

  // Which view this page is showing, kept in the URL rather than in component
  // state: a view of a session is a thing you send someone, and it should
  // survive a reload.
  const [searchParams, setSearchParams] = useSearchParams();
  // Only on the session's own page. Scoped to one agent — a subagent, a sub session, a
  // workflow step — the transcript below is that agent's, while the roster is
  // still the whole session's: the map would label the open agent "main agent"
  // and draw its siblings hanging off it. A run already has its graph, which is
  // the structural view for a run.
  //
  // Three views of the same session: its prose, its shape in time, and its
  // shape in lineage. Only one holds the pane at a time — they are answers to
  // the same question, not panels to arrange.
  //
  // The URL is what a link carries, so it wins when it names a view. What it
  // cannot say is what *this* person was doing: opening the next session from
  // the rail would drop someone who works in the timeline back into prose,
  // every time. So the last view they picked is remembered on this browser and
  // is what a session opens in when its URL says nothing.
  const [lastView, setLastView] = usePersistentState<SessionViewId>(
    "horsie-session-view",
    "transcript",
    { deserialize: (raw) => VIEWS.find((v) => v.id === raw)?.id },
  );
  // A session started a second ago is not one you *opened*: it is the answer
  // to a message just typed, so it lands in the transcript whatever the
  // remembered view is.
  const fresh = (useLocation().state as { fresh?: boolean } | null)?.fresh === true;
  const asked = searchParams.get("view");
  const view: SessionViewId =
    agentId || fresh
      ? "transcript"
      : (VIEWS.find((v) => v.id === asked)?.id ?? lastView);
  const timelineOpen = view === "timeline";
  const graphOpen = view === "graph";
  /** Whether anything has taken the pane from the transcript. */
  const overlayOpen = timelineOpen || graphOpen;
  const showView = (next: SessionViewId) => {
    setLastView(next);
    setSearchParams(
      (prev) => {
        const params = new URLSearchParams(prev);
        if (next === "transcript") params.delete("view");
        else params.set("view", next);
        return params;
      },
      { replace: true },
    );
  };
  // A remembered view still has to reach the URL, or the page would show one
  // thing and the link in the address bar would promise another.
  useEffect(() => {
    if (agentId || asked === view || view === "transcript") return;
    setSearchParams(
      (prev) => {
        const params = new URLSearchParams(prev);
        params.set("view", view);
        return params;
      },
      { replace: true },
    );
  }, [agentId, asked, view, setSearchParams]);
  /** An entry the timeline asked for, held until the transcript is on screen
   * again — its anchors do not exist while the timeline has the pane. */
  const [pendingSeek, setPendingSeek] = useState<string | null>(null);

  /** Agents whose own work is being drawn on their lane, and the histories
   * fetched for them.
   *
   * On demand rather than up front: a session with a dozen subagents would be a
   * dozen requests on open, to draw detail nobody had asked to see yet. One
   * read per lane, the first time it is opened, kept for as long as the page
   * lives. */
  const [expanded, setExpanded] = useState<string[]>([]);
  /** Agents whose children are folded away. */
  const [collapsed, setCollapsed] = useState<string[]>([]);
  const [histories, setHistories] = useState<Record<string, TranscriptItem[]>>({});

  const toggleExpand = async (agentId: string) => {
    if (expanded.includes(agentId)) {
      setExpanded((prev) => prev.filter((a) => a !== agentId));
      return;
    }
    setExpanded((prev) => [...prev, agentId]);
    if (histories[agentId] || !id) return;
    try {
      const page = await api.sessions.messages(id, agentId, { max: 200 });
      // `false, false`: a fetched history is a finished read, so nothing in it
      // is still in flight, and it is the whole of what there is to page.
      setHistories((prev) => ({ ...prev, [agentId]: transcriptItems(page.entries, false, false) }));
    } catch {
      // The lane stays a span. A failed read must not take the view with it.
      setExpanded((prev) => prev.filter((a) => a !== agentId));
    }
  };

  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  // When a scroll-back page is loading, holds the scroll height captured just
  // before the prepend so we can restore the viewport position after it lands.
  const loadAnchor = useRef<number | null>(null);

  // A sub session this session just branched, not yet opened. Held rather than
  // navigated to at once, because a sub session is `provisioning` until its history
  // has been handed over.
  const [pendingSubSession, setPendingSubSession] = useState<string | null>(null);
  useEffect(() => {
    if (!pendingSubSession || !id) return;
    if (!subSessionReadyToOpen(detail?.subSessions, pendingSubSession)) return;
    setPendingSubSession(null);
    navigate(`/sessions/${id}/agents/${pendingSubSession}`);
  }, [pendingSubSession, detail?.subSessions, id, navigate]);

  const handleSend = async (sessionId: string, text: string) => {
    setSendError(null);
    // Echo the message immediately — a live session's SSE push for this same
    // message can arrive before this request resolves, so the echo must exist
    // *before* the request goes out or the real message beats it and the
    // echo is left stuck as an unmatched duplicate.
    const optimisticId = addOptimisticUser(text);
    try {
      const ack = await send.mutateAsync({ id: sessionId, text, agentId });
      // From here the server owns the message: the echo is handed its
      // server-side id so the queue can take it over without duplicating it.
      ackOptimisticUser(optimisticId, ack.messageId);
      // `/fork` and `/summary-n-fork` create a session and answer with it.
      // The message belongs to that session, not this one, so the echo
      // goes with it — leaving it here would show the command as something
      // said in the session it branched away from.
      // Not navigated yet: a `/summary-n-fork` is a turn on *this* session,
      // and the sub session has no history until that turn produces the summary.
      // Landing there now would open a blank transcript for as long as a
      // provider call takes. The effect below moves us when it is ready — for a
      // `/fork`, whose seed is a local copy, that is the very next frame.
      if (ack.subSession) {
        removeOptimisticUser(optimisticId);
        setPendingSubSession(ack.subSession);
      }
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

  // `undefined` only until the session document arrives. The server always has
  // a status — it keeps a durable copy — so this is the client not knowing yet,
  // not the server having nothing to say.
  const status = stream.liveStatus ?? detail?.status ?? undefined;
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
  // Every compaction boundary in the transcript, oldest first — what the spine
  // puts a tick on.
  const boundaries = stream.items
    .filter((i) => i.kind === "compaction")
    .map((i) => i.value);

  // `Date.now()` is read here rather than inside the builder so one layout pass
  // measures every still-running bar against the same instant.
  const timeline = useMemo(
    () =>
      buildTimeline(
        stream.items,
        detail?.agents ?? [],
        detail?.subSessions ?? [],
        Date.now(),
        histories,
      ),
    [stream.items, detail?.agents, detail?.subSessions, histories],
  );

  // Both rosters, because the graph is the session's whole lineage: its agents
  // and the sessions branched from it. The same `collapsed` list the timeline
  // reads — folding something is a statement about it, not about the view it
  // was folded in.
  const agentTree = useMemo(
    () => layoutAgentTree(detail?.agents ?? [], detail?.subSessions ?? [], collapsed),
    [detail?.agents, detail?.subSessions, collapsed],
  );

  /** Scroll to a transcript entry by id, to a boundary by seq, or to either end. */
  const seek = (target: number | string) => {
    const el = scrollRef.current;
    if (!el) return;
    if (target === "start") {
      // Scrolling to the top is not the same as reaching the start of the
      // session: history pages in, so the first thing rendered may not be the
      // first thing there is. This goes as far back as has loaded, and the
      // existing scroll-back handler fetches the rest.
      el.scrollTo({ top: 0, behavior: "smooth" });
      return;
    }
    if (target === "end") {
      el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
      return;
    }
    // A compaction boundary is addressed by its seq, and the spine has always
    // sought it that way. Both a spine tick and a timeline tick arrive here.
    const divider = el.querySelector(
      `[data-testid="compaction-divider"][data-seq="${target}"]`,
    );
    if (divider) {
      divider.scrollIntoView({ behavior: "smooth", block: "center" });
      return;
    }
    // Otherwise a message id, from a timeline bar. A turn declares every
    // message it folded together, so `~=` finds the block holding this one.
    const entry = el.querySelector(`[data-entry-ids~="${CSS.escape(String(target))}"]`);
    if (entry) {
      entry.scrollIntoView({ behavior: "smooth", block: "center" });
      return;
    }
    // Neither: the entry has been paged out. `seek("start")` has the same
    // limitation and answers it the same way — go as far back as has loaded and
    // let the scroll-back handler fetch the rest.
    el.scrollTo({ top: 0, behavior: "smooth" });
  };

  const [scrolledUnder, setScrolledUnder] = useState(false);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    // Only on the flip, so a long transcript does not re-render per frame.
    const under = el.scrollTop > 2;
    setScrolledUnder((prev) => (prev === under ? prev : under));
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 96;
    if (el.scrollTop < 80 && stream.hasMoreBefore && !stream.loadingMore) {
      loadAnchor.current = el.scrollHeight;
      loadMore();
    }
  };
  useLayoutEffect(() => {
    const el = scrollRef.current;
    // Not while the timeline or the graph has the pane: the transcript is `display: none`
    // there, so every measurement below reads zero and would leave it parked at
    // the top. Re-running when the pane comes back is what restores it, which
    // is why `overlayOpen` is a dependency rather than just a guard.
    if (!el || overlayOpen) return;
    // A just-completed scroll-back prepend: keep the viewport where it was by
    // pushing down by exactly the height the older messages added.
    if (loadAnchor.current != null) {
      el.scrollTop += el.scrollHeight - loadAnchor.current;
      loadAnchor.current = null;
      return;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  }, [stream.items, stream.streaming, stream.orphanTools.length, overlayOpen]);

  // A bar was clicked. Runs after the layout effect above has put the
  // transcript back where it was, so this lands on top of it rather than
  // fighting it.
  useEffect(() => {
    if (overlayOpen || pendingSeek === null) return;
    seek(pendingSeek);
    setPendingSeek(null);
  }, [overlayOpen, pendingSeek]);

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

  // The agent this page is showing, which is the one whose turn the button
  // interrupts. Unscoped, it always meant the main agent: on a sub session's page it
  // cancelled a turn the reader was not looking at, or — once the sub session was what
  // was running — did nothing at all and said `200`.
  const handleStop = async () => {
    try {
      await stop.mutateAsync({ id, agentId: agentId ?? MAIN_AGENT });
    } catch {
      /* surfaced via status */
    }
  };

  // Deletes what you are looking at. A sub session used to be deletable only
  // from its row in the rail, which no longer lists them — and a control that
  // deleted the whole session from a sub session's page would be the wrong
  // thing under the same key.
  const handleDelete = async () => {
    if (openSubSession) {
      const name = openSubSession.title ?? "this sub session";
      if (!(await askConfirm(`Delete “${name}”? This cannot be undone.`))) return;
      try {
        await delSubSession.mutateAsync({ id, subSessionId: openSubSession.id });
        navigate(`/sessions/${id}`);
      } catch {
        /* reported by the global failure notice */
      }
      return;
    }
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

  // A run has no single session, so the graph is its page. Opening one of
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
          <header
            data-scrolled={scrolledUnder ? "true" : undefined}
            className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6"
          >
            <RailToggle />
            {status && <StatusLamp status={status} />}
            {openSubSession ? (
              <SubSessionTitle
                sessionId={id}
                sessionName={detail?.name}
                subSession={openSubSession}
              />
            ) : (
              <SessionTitle name={detail?.name} />
            )}
            {/* Beside the title rather than in the key cluster on the right:
                this changes *what you are looking at*, and that cluster is for
                acting on what you are already looking at. */}
            {!agentId && (
              <div
                className="flex shrink-0 items-center gap-0.5 rounded-[var(--radius-control)] bg-screen p-0.5"
                role="radiogroup"
                aria-label="View"
                data-testid="view-switch"
              >
                {/* One control with three settings rather than two independent
                    toggles: the views are answers to the same question, and
                    exactly one of them always holds the pane. The transcript
                    is a setting like the others — it was the only view you
                    reached by un-pressing something, which said "off" where it
                    meant "prose". */}
                {VIEWS.map((v) => (
                  <button
                    key={v.id}
                    type="button"
                    role="radio"
                    aria-checked={view === v.id}
                    className={cn(
                      "key-icon shrink-0 !h-7 !w-7",
                      // A recessed trough with one key standing proud of it:
                      // the selected view is a raised key, not a tinted one.
                      view === v.id
                        ? "!bg-panel !text-legend shadow-[var(--float)]"
                        : "hover:!bg-raised",
                    )}
                    onClick={() => showView(v.id)}
                    title={v.title}
                    aria-label={v.label}
                    data-testid={v.testId}
                  >
                    <v.icon size={15} aria-hidden />
                  </button>
                ))}
              </div>
            )}
            {/* Durability is the product's whole differentiator, so a dropped
                feed is a first-class state on the panel — not a transcript
                that quietly stops moving while the lamp still says Running. */}
            {!stream.connected && (
              <span
                className="flex shrink-0 items-center gap-2 text-live-ink"
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
              {/* The session, or the sub session whose page this is. A step is
                  not deletable on its own — the run is the unit, and its page
                  carries the control — and neither is a subagent, which its
                  parent owns. */}
              {(!agentId || openSubSession) && (
                <button
                  className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
                  onClick={handleDelete}
                  disabled={del.isPending || delSubSession.isPending}
                  title={openSubSession ? "Delete sub session" : "Delete session"}
                  aria-label={openSubSession ? "Delete sub session" : "Delete session"}
                  data-testid="session-delete"
                >
                  <Trash2 size={15} aria-hidden />
                </button>
              )}
            </div>
          </header>

          {/* The session's shape instead of its prose. The composer and the
              config bar below stay put, so a session can still be driven while
              the map is up. */}
          {timelineOpen && (
            <SessionPane>
              <SessionTimeline
                timeline={timeline}
                expanded={expanded}
                collapsed={collapsed}
                onToggleCollapse={(agent) =>
                  setCollapsed((prev) =>
                    prev.includes(agent) ? prev.filter((a) => a !== agent) : [...prev, agent],
                  )
                }
                onToggleExpand={(agent) => void toggleExpand(agent)}
                onSelectEntry={(entryId) => {
                  // Reading an entry means reading the transcript. Switch back
                  // and record where to go; the effect above seeks once the
                  // transcript has actually rendered its anchors.
                  setPendingSeek(entryId);
                  showView("transcript");
                }}
                onSelectAgent={(agent) => navigate(`/sessions/${id}/agents/${agent}`)}
              />
            </SessionPane>
          )}

          {/* The same roster the timeline lays along an axis, laid along its
              lineage instead. */}
          {graphOpen && (
            <SessionPane>
              <AgentGraph
                tree={agentTree}
                onToggleCollapse={(agent) =>
                  setCollapsed((prev) =>
                    prev.includes(agent) ? prev.filter((a) => a !== agent) : [...prev, agent],
                  )
                }
                onSelectAgent={(agent) => navigate(`/sessions/${id}/agents/${agent}`)}
              />
            </SessionPane>
          )}

          {/* Transcript */}
          <SessionPane
            scroll
            ref={scrollRef}
            onScroll={onScroll}
            data-testid="transcript-scroll"
            className={cn(overlayOpen && "hidden")}
          >
            {/* Inside the scroller so the spine's own `sticky` keeps it in
                view; outside it there would be nothing to stick to. */}
            <TranscriptSpine boundaries={boundaries} onSeek={seek} />
            {isLoading && stream.items.length === 0 ? (
              <div className="flex h-full items-center justify-center gap-2">
                <span className="lamp lamp-live text-live-ink" aria-hidden />
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
                  {status && statusMeta(status).hint}
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
                          className="lamp lamp-live text-live-ink"
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
                  sessionId={id}
                />
              </>
            )}
          </SessionPane>

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
                <span className="lamp lamp-live text-live-ink" aria-hidden />
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

          {/* Composer, and the channels it runs on. Both belong to the
              transcript: the timeline and the graph are pictures of the
              session rather than the conversation, and an input wired to
              something you are not reading is an invitation to type into the
              wrong place. The stop control lives on the composer and goes with
              it — the transcript is one key away, and the header keeps the
              status lamp either way. */}
          {/* The channels this session runs on, in the same place the draft
              row occupied before it existed. Read-only — each key opens its
              value rather than a picker. */}
          {!overlayOpen && detail && mainAgent && (
            <SessionConfigBar mode="locked" detail={detail} agent={mainAgent} />
          )}
          {/* A workflow step takes no messages — the definition drives it — so
              it gets the stop control without the send one. */}
          {overlayOpen ? null : agentId && detail?.workflow ? (
            <div className="bar-scroll flex items-center gap-3 px-4 py-2">
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
