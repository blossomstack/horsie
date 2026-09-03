import type { LucideIcon } from "lucide-react";
import {
  ChartNoAxesGantt,
  CircleAlert,
  ListTodo,
  MessageSquareText,
  SendHorizontal,
  Square,
  Trash2,
  Waypoints,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useLocation, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { ApiRequestError, MAIN_AGENT, api } from "../api/client";
import { subSessionReadyToOpen } from "../lib/subSessionTree";
import {
  SessionStatusKind,
  TaskStatus,
  type ArtifactRef,
  type StepRunView,
  type WorkflowRunGraph,
} from "../api/types";
import { AskAnswerProvider } from "../components/AskUserCard";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { ContextGauge } from "../components/ContextGauge";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { AgentGraph } from "../components/AgentGraph";
import { AgentInfoPanel, selectAgent } from "../components/AgentInfoPanel";
import { EntryInfoPanel } from "../components/EntryInfoPanel";
import { SessionPane } from "../components/SessionPane";
import { SessionTimeline } from "../components/SessionTimeline";
import { SettingsMenu } from "../components/SettingsMenu";
import { StatusLamp } from "../components/StatusBadge";
import { TaskListPanel } from "../components/TaskListPanel";
import { Transcript } from "../components/Transcript";
import {
  formatTranscriptComments,
  type TranscriptComment,
} from "../components/TranscriptComments";
import { TranscriptSpine } from "../components/TranscriptSpine";
import { formatOutput, retryUnavailable } from "./workflows/runGraph";
import { useRetryStep, useWorkflowRun } from "../hooks/useWorkflows";
import { askConfirm } from "../lib/confirm";
import { usePersistentState } from "../hooks/usePersistentState";
import { transcriptItems, useSessionStream } from "../hooks/useSessionStream";
import type { TranscriptItem } from "../hooks/useSessionStream";
import { useEntryCatalog } from "../hooks/useEntryCatalog";
import { useUiSettings } from "../hooks/useUiSettings";
import {
  qk,
  useDeleteSession,
  useDeleteAgent,
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
import { isSessionsOwnPage } from "../lib/sessionRoute";
import {
  progressionLabel,
  settled,
  showsProgression,
  statusMeta,
} from "../lib/status";
import { Trans, useTranslation } from "react-i18next";

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
  const { t } = useTranslation();
  const gone = error instanceof ApiRequestError && error.status === 404;
  return (
    <div className="flex h-full flex-col" data-testid="session-unavailable">
      <header className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 data-testid="session-title" className="page-title min-w-0 flex-1 truncate">
          {gone ? t("session.noSuch") : t("session.loadFailed")}
        </h1>
      </header>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          <section className="section">
            <h2 className="legend">{t("session.sessionId")}</h2>
            <pre className="screen mt-3 overflow-x-auto px-3 py-2.5 font-mono text-[0.6875rem] leading-relaxed text-legend select-all">
              {id}
            </pre>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              {gone
                ? t("session.goneHint")
                : error instanceof Error
                  ? error.message
                  : t("session.readFailed")}{" "}
              <Trans
                i18nKey="session.yourSessionsList"
                components={{
                  lnk: <Link className="text-legend underline" to="/" />,
                }}
              />
            </p>
          </section>
        </div>
      </div>
    </div>
  );
}

/**
 * Where one agent sits in a run's log, if it is a step of one.
 *
 * The roster knows agent ids; a retry names a position in the run log. Only
 * the run's graph holds both, so this is the join — and its absence is what
 * says "this agent is not a workflow step", which is what decides whether a
 * Retry key is offered at all.
 */
function stepIndexOf(
  graph: WorkflowRunGraph | undefined,
  agentId: string,
): { index: number; step: string } | undefined {
  for (const node of graph?.nodes ?? []) {
    const run = node.runs.find((r) => r.agentId === agentId);
    if (run) return { index: run.index, step: node.step };
  }
  return undefined;
}

/** The execution one agent *is*, for the rule that decides whether retrying it
 * would race a step already writing the workspace. */
function stepRunOf(
  graph: WorkflowRunGraph | undefined,
  agentId: string,
): StepRunView | undefined {
  for (const node of graph?.nodes ?? []) {
    const run = node.runs.find((r) => r.agentId === agentId);
    if (run) return run;
  }
  return undefined;
}

export function SessionView() {
  const { t } = useTranslation();
  const { id, agentId } = useParams<{ id: string; agentId?: string }>();
  const navigate = useNavigate();
  const { data: detail, isLoading, isError, error } = useSession(id);
  /**
   * Whether this page *is* a run, rather than a session that started one.
   *
   * A run used to have a page of its own, with its own header, its own
   * two-column layout and its own controls — a second session UI that had to
   * be kept in step with this one by hand, and drifted. It is a session: the
   * only true differences are that the graph it opens in is the *definition's*
   * graph rather than its agents' lineage, and that it has no transcript of
   * its own to fall back to. Both are said here rather than by a fork.
   */
  const isRun = !!detail?.workflow;
  /** A run's page, as opposed to one of its steps opened from it. */
  const runPage = isRun && !agentId;
  // The session's own bundles decide what `/` and `@` offer.
  const entries = useEntryCatalog(detail?.plugins);
  // No stream for a run: it has no `main` agent to read, so the connection
  // would carry nothing — and an open reader renews a session's idle clock,
  // so a run page left open would pin a finished run resident for as long as
  // the tab lived.
  const {
    stream,
    addOptimisticUser,
    removeOptimisticUser,
    ackOptimisticUser,
    loadMore,
  } = useSessionStream(runPage ? undefined : id, agentId ?? MAIN_AGENT);
  const { data: mainAgent } = useAgent(runPage ? undefined : id, agentId ?? MAIN_AGENT);
  const send = useSendMessage();
  const answerAsks = useAnswerAsks();
  const stop = useStopAgent();
  const del = useDeleteSession();
  const delAgent = useDeleteAgent();
  /** The run this page is scoped to, when it is scoped to one. A sub session
   *  and a subagent are both here: both can be named, and both can be
   *  removed. A workflow step is neither — the run owns it. */
  const openSubSession = agentId
    ? detail?.subSessions?.find((row) => row.id === agentId)
    : undefined;
  const openAgent = agentId
    ? detail?.agents?.find((row) => row.id === agentId)
    : undefined;
  /** What this page is called: the run's own title, never the session's with
   *  the run's appended. The main agent's title *is* the session's name. */
  const runTitle = agentId
    ? (openSubSession?.title ?? openAgent?.title ?? openAgent?.agentType)
    : detail?.name;
  /** Only a subagent's run and a sub session can go. The main agent is the
   *  session — its key deletes the session — and a workflow step belongs to
   *  its run's log. */
  const runDeletable =
    openSubSession != null || (openAgent != null && openAgent.kind === "subagent");
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
  // A scoped page picks its view like any other. It used to be pinned to the
  // transcript, because both structural views were the *session's* — drawn
  // from the main agent, with the whole roster hanging off it — so on a
  // subagent's page they showed the wrong thing over the right transcript.
  // Both are now drawn of whichever run the page is on.
  // A run has no transcript and no agent timeline of its own: it *is* its
  // steps, and each of those has both. So on a run's own page the graph is the
  // only view there is — it opens there whatever was remembered, and the other
  // two keys are disabled rather than hidden, because a run that showed one
  // key where every session shows three reads as a different kind of thing.
  // Open a step and all three come back, scoped to that step.
  const view: SessionViewId = runPage
    ? "graph"
    : fresh
      ? "transcript"
      : (VIEWS.find((v) => v.id === asked)?.id ?? lastView);
  const timelineOpen = view === "timeline";
  const graphOpen = view === "graph";
  /** Whether anything has taken the pane from the transcript. */
  const overlayOpen = timelineOpen || graphOpen;
  const showView = (next: SessionViewId) => {
    if (runPage) return;
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
    if (runPage || asked === view || view === "transcript") return;
    setSearchParams(
      (prev) => {
        const params = new URLSearchParams(prev);
        params.set("view", view);
        return params;
      },
      { replace: true },
    );
  }, [asked, view, setSearchParams]);
  /** An entry the timeline asked for, held until the transcript is on screen
   * again — its anchors do not exist while the timeline has the pane. */
  const [pendingSeek, setPendingSeek] = useState<string | null>(null);

  /**
   * What this run is called, when the session is a run of a workflow.
   *
   * Its own title first: a run is a session, so it is named like one and can
   * be renamed like one, and the workflow's name is only what that title
   * defaults to. Absent unless the session *is* a run — an ordinary session
   * whose agent invoked a workflow has steps in its roster too, and it is not
   * one.
   */
  const workflowRunTitle = detail?.workflow
    ? (detail.name?.trim() || detail.workflow)
    : undefined;


  /** The session's own agent, whose page is the session's page.
   *
   * A run has none: it *is* its steps, and `agents` lists one entry per step
   * execution — the first of which is rooted and at depth 0 and would be read
   * as the session's own agent. Opening the start step then routed to the
   * run's page instead of the step's, which is the page it was asked for. */
  const mainAgentId = isRun
    ? undefined
    : (detail?.agents?.find((a) => !a.parent && a.depth === 0)?.id ??
      detail?.agents?.[0]?.id ??
      MAIN_AGENT);

  /**
   * Leave a structural view for one run's transcript.
   *
   * Landing in the *transcript* is the whole point of the key, and it used not
   * to: the remembered view is what a page with nothing in its URL opens in,
   * so pressing "open this agent's transcript" from the timeline opened that
   * agent's timeline — the same picture, one run over, with the thing you
   * pressed for nowhere in sight. Reading a transcript is also a view someone
   * picked, so it is remembered like any other rather than smuggled past the
   * memory.
   *
   * The selection goes with it. It belongs to the picture being left, and
   * carrying it over meant arriving with a panel open on the run you had just
   * come from.
   */
  const openRun = (agent: string) => {
    setLastView("transcript");
    setSelection(null);
    // The run node is the session — and a workflow run's session page is its
    // graph, not a transcript, which is the one place worth landing from a
    // step. `RUN_ROOT` is not an agent, so it has no page of its own.
    const own = isSessionsOwnPage({
      agent,
      isRun,
      mainAgentId,
      mainAgentAlias: MAIN_AGENT,
    });
    navigate(own ? `/sessions/${id}` : `/sessions/${id}/agents/${agent}`);
  };

  /** Agents whose own work is being drawn on their lane, and the histories
   * fetched for them.
   *
   * On demand rather than up front: a session with a dozen subagents would be a
   * dozen requests on open, to draw detail nobody had asked to see yet. One
   * read per lane, the first time it is opened, kept for as long as the page
   * lives. */
  // The run this page is on starts expanded, because its history is already
  // here — it is the transcript above. Every other lane is fetched on demand.
  const [expanded, setExpanded] = useState<string[]>(agentId ? [agentId] : []);
  /** Agents whose children are folded away. */
  const [collapsed, setCollapsed] = useState<string[]>([]);
  /** What the panel beside a structural view is showing: one agent, or one
   *  entry. Held here rather than in either view because both write it and
   *  only one panel is ever open — and because switching between the two views
   *  should not lose what you had selected. */
  const [selection, setSelection] = useState<
    { kind: "agent" | "entry"; id: string } | null
  >(null);
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
  const [transcriptComments, setTranscriptComments] = useState<TranscriptComment[]>([]);
  const transcriptCommentScope = `${id ?? ""}:${agentId ?? MAIN_AGENT}`;
  const transcriptCommentScopeRef = useRef(transcriptCommentScope);
  transcriptCommentScopeRef.current = transcriptCommentScope;
  useEffect(() => {
    setTranscriptComments([]);
  }, [transcriptCommentScope]);
  useEffect(() => {
    if (!pendingSubSession || !id) return;
    if (!subSessionReadyToOpen(detail?.subSessions, pendingSubSession)) return;
    setPendingSubSession(null);
    navigate(`/sessions/${id}/agents/${pendingSubSession}`);
  }, [pendingSubSession, detail?.subSessions, id, navigate]);

  const handleSend = async (
    sessionId: string,
    text: string,
    artifacts: ArtifactRef[],
  ) => {
    const requestScope = `${sessionId}:${agentId ?? MAIN_AGENT}`;
    if (transcriptCommentScopeRef.current === requestScope) setSendError(null);
    // Echo the message immediately — a live session's SSE push for this same
    // message can arrive before this request resolves, so the echo must exist
    // *before* the request goes out or the real message beats it and the
    // echo is left stuck as an unmatched duplicate.
    const optimisticId = addOptimisticUser(text, artifacts);
    try {
      const ack = await send.mutateAsync({ id: sessionId, text, agentId, artifacts });
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
      if (transcriptCommentScopeRef.current === requestScope) {
        setSendError(
          e instanceof ApiRequestError ? e.message : "Failed to send message.",
        );
      }
      // Rethrown so the composer can restore what was typed. Swallowing it
      // here is what let an offline send clear the box and lose the message.
      throw e;
    }
  };

  const sendTranscriptComments = async () => {
    if (
      !id ||
      transcriptComments.length === 0 ||
      transcriptComments.some((item) => !item.comment.trim())
    )
      return;
    const sending = transcriptComments;
    const sendingScope = transcriptCommentScope;
    const text = formatTranscriptComments(sending, {
      intro: t("transcript.commentPrompt"),
      excerpt: t("transcript.excerpt"),
      comment: t("transcript.comment"),
    });
    // The outgoing set stops being editable at send; a failed request restores it.
    const sent = new Set(sending.map((item) => item.id));
    setTranscriptComments((current) =>
      current.filter((item) => !sent.has(item.id)),
    );
    try {
      await handleSend(id, text, []);
    } catch (error) {
      // A late failure from a page already left must not seed its text into the
      // session now on screen.
      if (transcriptCommentScopeRef.current === sendingScope) {
        setTranscriptComments((current) => [
          ...sending.filter(
            (item) => !current.some((saved) => saved.id === item.id),
          ),
          ...current,
        ]);
      }
      throw error;
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
  //
  // A run is the exception: its stream is scoped to `main`, and a run has no
  // main agent — each step is a turn on a different one. So the stream sees a
  // turn begin and never sees one end, and `liveStatus` sticks on `Running`
  // for the life of the page. The session document is the only thing that
  // knows a run has finished, so on a run's page it is the only thing asked.
  const status =
    (runPage ? detail?.status : (stream.liveStatus ?? detail?.status)) ??
    undefined;

  const { data: runGraph } = useWorkflowRun(runPage ? id : undefined, status);
  const retry = useRetryStep(id ?? "");
  // One more read when a run settles.
  //
  // The poll above stops on the settled status — but that status arrives by
  // being *pushed*: the global feed patches a session's summary straight into
  // this cache without fetching. So the news that the run has finished is what
  // cancels the fetch that would have collected its final steps, and a run
  // that finished fast left the graph reading "no agents recorded".
  //
  // The same shape `useWorkflowRun` uses for the same reason, where it is
  // expressed by keying the query on the status.
  const client = useQueryClient();
  useEffect(() => {
    if (!runPage || !id || !status || !settled(status)) return;
    void client.invalidateQueries({ queryKey: qk.session(id) });
  }, [runPage, id, status, client]);

  const retryStep = async (index: number, step: string) => {
    if (!(await askConfirm(t("run.confirmRetry", { step }), t("run.retry")))) return;
    retry.mutate(index);
  };

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
        // The page's own transcript, offered as this agent's history. On a
        // workflow run the root lane is the *run*, so the step being read
        // draws its bars on its own lane like any other member — from what is
        // already loaded, not from a second read of the same messages.
        agentId ? { ...histories, [agentId]: stream.items } : histories,
        // The run this page is scoped to. `stream.items` is already that
        // run's transcript, so drawing its bars on a lane labelled "main
        // agent" — with the whole session's roster hanging off it — was the
        // picture contradicting the prose it was drawn from.
        agentId,
        collapsed,
        workflowRunTitle,
      ),
    [
      stream.items,
      detail?.agents,
      detail?.subSessions,
      histories,
      agentId,
      collapsed,
      workflowRunTitle,
    ],
  );

  // Both rosters, because the graph is the session's whole lineage: its agents
  // and the sessions branched from it. The same `collapsed` list the timeline
  // reads — folding something is a statement about it, not about the view it
  // was folded in.
  const agentTree = useMemo(
    () =>
      layoutAgentTree(
        detail?.agents ?? [],
        detail?.subSessions ?? [],
        collapsed,
        workflowRunTitle,
      ),
    [detail?.agents, detail?.subSessions, collapsed, workflowRunTitle],
  );

  /** The selected agent, resolved against both rosters. */
  const selectedAgent = useMemo(() => {
    const agent =
      selection?.kind === "agent"
        ? selectAgent(
            selection.id,
            detail?.agents ?? [],
            detail?.subSessions ?? [],
            detail?.name,
            workflowRunTitle,
          )
        : null;
    // A run's result and its failure are the run node's own, and the panel
    // already has a place for both — it is where every step's result reads.
    // They used to be a banner above the graph, which is chrome no other
    // session has: the run's page stopped looking like a session's the moment
    // it grew a section of its own. The roster cannot supply them (a run's
    // output is in its graph, not in its agents), so they are attached here.
    if (!agent || agent.kind !== "run" || !runGraph) return agent;
    return {
      ...agent,
      output:
        runGraph.output === undefined || runGraph.output === null
          ? undefined
          : formatOutput(runGraph.output),
      error: runGraph.error,
    };
  }, [
    selection,
    detail?.agents,
    detail?.subSessions,
    detail?.name,
    workflowRunTitle,
    runGraph,
  ]);

  /** The selected entry's message. Found in what is loaded rather than
   *  fetched: the timeline only draws bars for entries it was handed, so a
   *  bar you can click is a message this page already holds. */
  const selectedEntry = useMemo(() => {
    if (selection?.kind !== "entry") return null;
    for (const item of stream.items) {
      if (item.kind === "message" && item.value.id === selection.id) return item.value;
    }
    for (const history of Object.values(histories)) {
      for (const item of history) {
        if (item.kind === "message" && item.value.id === selection.id) return item.value;
      }
    }
    return null;
  }, [selection, stream.items, histories]);

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
  const [contentBelow, setContentBelow] = useState(false);
  const [spine, setSpine] = useState({ view: 1, progress: 0 });

  /* Both edges of the transcript, from one measurement.
   *
   * The header takes a shadow when something has scrolled up under it; the
   * composer takes one when there is still transcript below the fold, hidden
   * behind it. Each only flips its own boolean, so a long transcript does not
   * re-render per frame while it scrolls. */
  const readEdges = (el: HTMLElement) => {
    const under = el.scrollTop > 2;
    setScrolledUnder((prev) => (prev === under ? prev : under));
    const below = el.scrollHeight - el.scrollTop - el.clientHeight > 2;
    setContentBelow((prev) => (prev === below ? prev : below));
    // The spine draws the scrollbar, so it works from the same two numbers a
    // native one does. Quantised to 1/500ths: a re-render per pixel of a long
    // transcript buys nothing a 3px thumb can show.
    const span = el.scrollHeight - el.clientHeight;
    const v = Math.round((el.clientHeight / Math.max(el.scrollHeight, 1)) * 500) / 500;
    const g = span > 0 ? Math.round((el.scrollTop / span) * 500) / 500 : 0;
    setSpine((prev) =>
      prev.view === v && prev.progress === g ? prev : { view: v, progress: g },
    );
  };

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    readEdges(el);
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

  useEffect(() => {
    const el = scrollRef.current;
    if (el && !overlayOpen) readEdges(el);
  });

  // Reset page-local state when switching sessions or agent runs.
  useEffect(() => {
    stick.current = true;
    setSendError(null);
  }, [transcriptCommentScope]);

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

  /** Remove one run, and everything it delegated. Used by the header key and
   *  by the panel beside the structural views, so the confirmation and the
   *  navigation afterwards are written once. */
  const deleteRun = async (runId: string, name: string) => {
    if (!(await askConfirm(t("session.confirmDeleteRun", { name })))) return;
    try {
      await delAgent.mutateAsync({ id, agentId: runId });
      // Back to the session, which still exists. Only when the page *was* that
      // run: deleting one from the graph leaves you where you were.
      if (agentId === runId) navigate(`/sessions/${id}`);
    } catch {
      /* reported by the global failure notice */
    }
  };

  // Deletes what you are looking at. A sub session used to be deletable only
  // from its row in the rail, which no longer lists them — and a control that
  // deleted the whole session from a sub session's page would be the wrong
  // thing under the same key.
  const handleDelete = async () => {
    if (runDeletable && agentId) {
      await deleteRun(agentId, runTitle ?? t("session.thisRun"));
      return;
    }
    if (!(await askConfirm(t("session.confirmDelete"))))
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
            {/* Whatever run this page is, named on its own terms. It used to
                put the parent session's name and a link in front of a sub
                session's title, which spent the widest thing in the header on
                context that the rail already carries — and did it for one kind
                of run only, so the header said different amounts about
                different agents. */}
            <SessionTitle name={runTitle} />
            {/* Beside the title rather than in the key cluster on the right:
                this changes *what you are looking at*, and that cluster is for
                acting on what you are already looking at. */}
            {/* On every run, not just the session's own. The three views are
                three readings of the same session, and a subagent's page had
                no way into either structural one — you had to go back to the
                session first, which is the navigation the graph exists to
                replace. */}
            {/* The trough with one key standing proud of it was the shared
                segmented control, hand-rolled here before there was one. */}
            <div
              className="segmented shrink-0"
              role="radiogroup"
              aria-label={t("session.view")}
              data-testid="view-switch"
            >
                {/* One control with three settings rather than two independent
                    toggles: the views are answers to the same question, and
                    exactly one of them always holds the pane. The transcript
                    is a setting like the others — it was the only view you
                    reached by un-pressing something, which said "off" where it
                    meant "prose". */}
                {VIEWS.map((v) => {
                  // Only the graph reads a run. The other two would be a
                  // transcript and a timeline of nothing — a run has no agent
                  // of its own — so they say why rather than showing empty.
                  const off = runPage && v.id !== "graph";
                  return (
                    <button
                      key={v.id}
                      type="button"
                      role="radio"
                      aria-checked={view === v.id}
                      disabled={off}
                      className="inline-flex h-7 w-7 shrink-0 items-center justify-center px-0"
                      onClick={() => showView(v.id)}
                      title={off ? t("run.noRunTranscript") : v.title}
                      aria-label={off ? t("run.noRunTranscript") : v.label}
                      data-testid={v.testId}
                    >
                      <v.icon size={15} aria-hidden />
                    </button>
                  );
                })}
            </div>
            {/* Durability is the product's whole differentiator, so a dropped
                feed is a first-class state on the panel — not a transcript
                that quietly stops moving while the lamp still says Running. */}
            {!runPage && !stream.connected && (
              <span
                className="flex shrink-0 items-center gap-2 text-live-ink"
                data-testid="session-reconnecting"
                title={t("session.reconnectingHint")}
              >
                <span className="lamp lamp-live" aria-hidden />
                <span className="legend hidden text-current sm:inline">
                  {t("session.reconnecting")}
                </span>
              </span>
            )}
            {/* Every key here acts on the transcript — its tokens, its plan,
                its display, its session — and none of them means anything
                while a structural view holds the pane.
                Disabled, not hidden. Hiding them kept the row from reflowing
                but left a hole where four keys had been, which reads as a
                rendering fault rather than as controls that do not apply.
                Greyed and unpressable says the same thing and looks like it
                meant to. */}
            <div className="flex shrink-0 items-center gap-0.5" data-testid="session-keys">
              {!isRun && transcriptComments.length > 0 && (
                <button
                  type="button"
                  className="key key-go key-sm mr-1"
                  disabled={
                    overlayOpen ||
                    send.isPending ||
                    transcriptComments.some((item) => !item.comment.trim()) ||
                    (status !== undefined && !statusMeta(status).canSend)
                  }
                  onClick={() => void sendTranscriptComments().catch(() => {})}
                  title={t("transcript.sendComments", {
                    count: transcriptComments.length,
                  })}
                  aria-label={t("transcript.sendComments", {
                    count: transcriptComments.length,
                  })}
                  data-testid="send-transcript-comments"
                >
                  <SendHorizontal size={13} aria-hidden />
                  <span className="hidden sm:inline">
                    {t("transcript.sendComments", { count: transcriptComments.length })}
                  </span>
                  <span className="sm:hidden">
                    {t("transcript.sendCommentsShort", {
                      count: transcriptComments.length,
                    })}
                  </span>
                </button>
              )}
              <ContextGauge
                agent={mainAgent}
                sessionTotal={detail?.usageTotal}
                disabled={overlayOpen}
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
                  // Three states have to stay apart. That a plan EXISTS is a
                  // ring and full ink; that the panel is OPEN is the wash
                  // and the edge every held key wears, off `aria-pressed`
                  // below; hover is the neutral fill. None of the three is
                  // written out here any more.
                  tasks.length > 0 &&
                    "!text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]",
                )}
                disabled={overlayOpen}
                onClick={() => setTasksOpen(!tasksOpen)}
                aria-pressed={tasksOpen}
                title={
                  tasks.length
                    ? t(
                        tasksOpen ? "taskList.hideWithCount" : "taskList.showWithCount",
                        { done: tasksDone, total: tasks.length },
                      )
                    : t(tasksOpen ? "taskList.hide" : "taskList.show")
                }
                aria-label={
                  tasks.length
                    ? t("taskList.toggleWithCount", {
                        done: tasksDone,
                        total: tasks.length,
                      })
                    : t("taskList.toggle")
                }
                data-testid="task-list-toggle"
                data-has-plan={tasks.length > 0 ? "true" : undefined}
              >
                <ListTodo size={15} aria-hidden />
              </button>
              <SettingsMenu disabled={overlayOpen} />
              {/* The session, or whichever run this page is. A workflow step
                  is the one thing with no key: the run is the unit, and its
                  page carries the control. */}
              {(!agentId || runDeletable) && (
                <button
                  className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
                  onClick={handleDelete}
                  disabled={overlayOpen || del.isPending || delAgent.isPending}
                  title={
                    !agentId
                      ? "Delete session"
                      : openSubSession
                        ? "Delete sub session"
                        : "Delete this subagent run"
                  }
                  aria-label={!agentId ? "Delete session" : "Delete this run"}
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
                // Beside the picture, not instead of it: a bar is unreadable
                // on its own, and switching to the transcript to identify one
                // closed the view that raised the question. Reading it in
                // place is the panel's own key.
                onSelectEntry={(entryId) => setSelection({ kind: "entry", id: entryId })}
                onSelectAgent={(agent) => setSelection({ kind: "agent", id: agent })}
                onOpenAgent={openRun}
                selectedAgent={selection?.kind === "agent" ? selection.id : undefined}
                selectedEntry={selection?.kind === "entry" ? selection.id : undefined}
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
                onSelectAgent={(agent) => setSelection({ kind: "agent", id: agent })}
                onOpenAgent={openRun}
                selected={selection?.kind === "agent" ? selection.id : undefined}
                // The run being read. On the session's own page that is its
                // main agent, which the graph draws as the root — `agentId` is
                // absent there, and a root that never lit up was the picture
                // failing to say "you are here" on the one page everyone
                // starts from.
                current={agentId ?? mainAgentId}
              />
            </SessionPane>
          )}

          {/* Transcript. The pane scrolls; the spine is pinned to it from
              outside, so it stays put while the transcript moves under it. */}
          <div className={cn("relative flex min-h-0 flex-1", overlayOpen && "hidden")}>
          <SessionPane
            scroll
            ref={scrollRef}
            onScroll={onScroll}
            data-testid="transcript-scroll"
          >
            {isLoading && stream.items.length === 0 ? (
              <div className="flex h-full items-center justify-center gap-2">
                <span className="lamp lamp-live text-live-ink" aria-hidden />
                <span className="legend">{t("session.loadingTranscript")}</span>
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
                        <span className="legend">{t("session.loadingEarlier")}</span>
                      </>
                    ) : (
                      <span className="legend">
                        {t("session.scrollUp")}
                      </span>
                    )}
                  </div>
                )}
                <Transcript
                  key={`${id}:${agentId ?? MAIN_AGENT}`}
                  items={stream.items}
                  streaming={stream.streaming}
                  orphanTools={stream.orphanTools}
                  showLive={status === SessionStatusKind.Running}
                  showThinking={uiSettings.showThinking}
                  sessionId={id}
                  commenting={
                    isRun
                      ? undefined
                      : {
                          comments: transcriptComments,
                          onAdd: (comment) =>
                            setTranscriptComments((current) => [...current, comment]),
                          onUpdate: (commentId, comment) =>
                            setTranscriptComments((current) =>
                              current.map((item) =>
                                item.id === commentId ? { ...item, comment } : item,
                              ),
                            ),
                          onRemove: (commentId) =>
                            setTranscriptComments((current) =>
                              current.filter((item) => item.id !== commentId),
                            ),
                        }
                  }
                />
              </>
            )}
          </SessionPane>
            <TranscriptSpine
              boundaries={boundaries}
              onSeek={seek}
              view={spine.view}
              progress={spine.progress}
              onScrollTo={(f) => {
                const el = scrollRef.current;
                if (!el) return;
                el.scrollTop = f * (el.scrollHeight - el.clientHeight);
              }}
            />
          </div>

          {/* Errors */}
          {(sendError || stream.streamError) && (
            <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
              <div
                data-testid="session-error"
                className="notice notice-fault"
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
                className="notice notice-fault"
              >
                <CircleAlert size={16} className="mt-0.5 shrink-0" />
                <div className="notice-body">
                  <p>{t("session.terminal", { reason: terminal })}</p>
                  <button
                    type="button"
                    className="key key-flat mt-2"
                    onClick={() => navigate("/")}
                    data-testid="session-terminal-new"
                  >
                    {t("rail.newSession")}
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
          <div
            className="bar-scroll-up shrink-0"
            data-scrolled={!overlayOpen && contentBelow ? "true" : undefined}
          >
          {!overlayOpen && detail && mainAgent && (
            <SessionConfigBar mode="locked" detail={detail} agent={mainAgent} />
          )}
          {/* A run takes no messages either — the definition drives every step
              — so its bar is the step bar's shape: what the page is, and the
              one control that acts on it. It sits below the graph rather than
              in the header because that is where a session's controls are, and
              a run is a session. */}
          {runPage ? (
            <div
              className="bar-scroll flex items-center gap-3 px-4 py-2"
              data-testid="run-bar"
            >
              <span className="text-xs text-faint">{t("run.runHint")}</span>
              {/* Only while something can still change on its own. A settled
                  run is moved by a retry, which is on the step. */}
              {!settled(status ?? SessionStatusKind.Idle) && (
                <button
                  className="key key-stop ml-auto key-sm"
                  onClick={handleStop}
                  data-testid="run-stop"
                >
                  <Square size={13} />
                  {t("run.interrupt")}
                </button>
              )}
            </div>
          ) : /* A workflow step takes no messages — the definition drives it —
                 so it gets the stop control without the send one. */
          overlayOpen ? null : agentId && detail?.workflow ? (
            <div className="bar-scroll flex items-center gap-3 px-4 py-2">
              <span className="text-xs text-faint">
{t("session.workflowStepHint")}
              </span>
              {/* Only while there is something to interrupt. The step's own
                  document says what became of it; offering the control on a
                  step that concluded hours ago was the same lie the badge told
                  beside it. */}
              {mainAgent?.status === "running" && (
                <button
                  className="key key-stop ml-auto key-sm"
                  onClick={handleStop}
                  data-testid="step-stop"
                >
                  {t("run.interrupt")}
                </button>
              )}
            </div>
          ) : (
            <Composer
              status={status}
              busy={send.isPending}
              entries={entries}
              onSend={(text, artifacts) => handleSend(id, text, artifacts)}
              onStop={handleStop}
            />
          )}
          </div>
        </div>

        {/* One column, three things that can be in it. The plan belongs to
            the transcript and the other two to the structural views, so they
            cannot collide — but the ordering is written out rather than left
            to whichever condition happens to be true first. */}
        {overlayOpen && selectedAgent ? (
          <AgentInfoPanel
            agent={selectedAgent}
            onClose={() => setSelection(null)}
            onOpenTranscript={openRun}
            onDelete={
              selectedAgent.kind === "subagent" || selectedAgent.kind === "sub_session"
                ? (agent) => void deleteRun(agent, selectedAgent.title)
                : undefined
            }
            deleting={delAgent.isPending}
            // A workflow step is the one agent that can be run again: the
            // definition still says what it was for. `stepIndex` is the run
            // log's position, which only the run's graph knows — the roster
            // has agent ids and nothing else.
            onRetry={
              stepIndexOf(runGraph, selectedAgent.id) === undefined
                ? undefined
                : (agentId) => {
                    const at = stepIndexOf(runGraph, agentId);
                    if (at) void retryStep(at.index, at.step);
                  }
            }
            retryBlocked={retryUnavailable(
              status ?? SessionStatusKind.Idle,
              retry.isPending,
              stepRunOf(runGraph, selectedAgent.id),
            )}
          />
        ) : overlayOpen && selectedEntry ? (
          <EntryInfoPanel
            message={selectedEntry}
            onClose={() => setSelection(null)}
            onOpenTranscript={(entryId) => {
              // Reading it in place means reading the transcript. Switch back
              // and record where to go; the effect above seeks once the
              // transcript has actually rendered its anchors.
              setPendingSeek(entryId);
              setSelection(null);
              showView("transcript");
            }}
          />
        ) : tasksOpen ? (
          <TaskListPanel tasks={tasks} onClose={() => setTasksOpen(false)} />
        ) : null}
      </div>
    </AskAnswerProvider>
  );
}
