import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import { MAIN_AGENT, api } from "../api/client";
import {
  Role,
  SessionStatusKind,
  type ContentPart,
  type HistoryEntry,
  type HistoryPage,
  type HookRecord,
  type Message,
  type PendingAskView,
  type QueuedMessage,
  type AgentStreamEvent,
  type SessionEvent,
  type TaskItem,
} from "../api/types";
import { toolScope } from "../lib/hookSummary";
import { qk, useAgent, useSession } from "./useSessions";

/** Messages per history page (initial tail and each scroll-back load). */
const HISTORY_LIMIT = 50;

// ---- View model handed to the UI -------------------------------------------

export interface RenderedToolCall {
  id: string;
  name: string;
  input: unknown;
  output?: string;
  isError?: boolean;
  running: boolean;
  /** Server stamp for when the tool finished; absent while it still runs. */
  endedAtMs?: number;
  /** What plugin hooks did to this call, in the order they ran. Empty for the
   * overwhelmingly common case of a call no hook matched. */
  hooks: HookRecord[];
}

/** One finished subagent's report, as the transcript renders it. */
export interface RenderedSubAgent {
  subagentId: string;
  label: string;
  /** "completed" | "failed". Anything else renders neutral — an unrecognized
   * status must not borrow success or failure styling it hasn't earned. */
  status: string;
  text: string;
  /** Zero for subagents journaled before spans were recorded; the row then
   * shows no duration rather than one invented from a missing stamp. */
  spawnedAtMs: number;
  endedAtMs: number;
}

export interface RenderedMessage {
  id: string;
  role: "User" | "Assistant";
  text: string;
  thinking: string[];
  toolCalls: RenderedToolCall[];
  /** Finished subagents whose results this message delivered. */
  subagentResults: RenderedSubAgent[];
  /** Server stamp for when this message was finalized. Absent on a message
   * this tab invented (an optimistic echo, a queued entry) — those have no
   * server time yet, and guessing one with the local clock would put them out
   * of order against the stamps around them. */
  createdAtMs?: number;
  /** Server stamp for when the provider call began (assistant only). */
  startedAtMs?: number;
  optimistic?: boolean;
  /** Accepted by the server but not yet carried into a turn. Rendered as
   * unread: without the marker, "stop, then the queued message immediately
   * starts a new turn" reads as the UI sending something on its own. */
  queued?: boolean;
}

/** A hook record with no tool call of its own, as the transcript renders it. */
export interface RenderedHookNotice {
  id: string;
  record: HookRecord;
  atMs: number;
}

/** One item in the rendered transcript.
 *
 * A union above `RenderedMessage` for the same reason `HistoryEntry` is one
 * above `Message`: not everything in a transcript is something the model saw,
 * and a notice must not have to pretend to be a message to be rendered. */
export type TranscriptItem =
  | { kind: "message"; value: RenderedMessage }
  | { kind: "notice"; value: RenderedHookNotice };

export interface SessionStream {
  items: TranscriptItem[];
  /** Live, not-yet-finalized assistant text (from Delta events). */
  streaming: string;
  /** Tools started but not yet attached to a finalized assistant message. */
  orphanTools: RenderedToolCall[];
  liveStatus: SessionStatusKind | null;
  /** The asks the live status says are waiting, or null when no status frame
   * has arrived yet — in which case the session detail is the answer. */
  livePendingAsks: PendingAskView[] | null;
  /** Incremented on every `StatusChanged` frame — including one that reports
   * the *same* status. The server reports without deduping, so this is how a
   * consumer observes "the session said something about its state" rather than
   * "the state differs from last render". */
  statusSeq: number;
  statusReason: string | null;
  streamError: string | null;
  connected: boolean;
  /** The agent's `task_list` tool state; empty until the tool is first used. */
  tasks: TaskItem[];
  /** Older messages exist before the currently-loaded window. */
  hasMoreBefore: boolean;
  /** A scroll-back page load is in flight. */
  loadingMore: boolean;
  /** The current resource-preparation stage while a turn spins up, or null. */
  progression: { stage: string; detail: string | null } | null;
}

// ---- Normalized reducer state ----------------------------------------------

interface StoredMessage {
  id: string;
  role: "User" | "Assistant";
  text: string;
  thinking: string[];
  toolCalls: { id: string; name: string; input: unknown }[];
  subagentResults: RenderedSubAgent[];
  createdAtMs: number;
  startedAtMs?: number;
}

/** One tool call's outcome, keyed by tool-call id. `atMs` is the server's
 * stamp for when the tool finished — it arrives on the `ToolResult` event
 * live, and as the tool-result message's `createdAtMs` on replay. */
interface ToolResultEntry {
  output: string;
  isError: boolean;
  atMs: number;
}

interface State {
  order: string[];
  byId: Record<string, StoredMessage>;
  toolResults: Record<string, ToolResultEntry>;
  liveTools: Record<string, { name: string; running: boolean }>;
  /** Hook records by the tool call they guarded. Keyed rather than ordered
   * because a record names its call: the server guarantees a record is
   * journaled before its tool result, but a client that reconnects mid-turn
   * may still see them in either order. */
  hooks: Record<string, HookRecord[]>;
  /** Hook records with no tool call — a `SessionStart` bootstrap, a `Stop` that
   * kept the turn going. These have nowhere to attach, so they are transcript
   * items of their own, keyed by the entry id that also orders them. */
  notices: Record<string, RenderedHookNotice>;
  /** Entry ids already folded, for both halves above. A hook entry can arrive
   * twice — once live, once on a backfill that overlaps the same window — and
   * the server's derived id is what makes the two recognizable as one. */
  hookEntryIds: Record<string, true>;
  /** Local echoes of messages this tab sent, shown until the server's own
   * account of them arrives — either in the queue or in the transcript.
   * `serverId` is the id the send was acknowledged with, once it resolves. */
  optimistic: { id: string; text: string; serverId?: string }[];
  /** The server's queue. Seeded from the session detail and kept live by
   * `InboxChanged`; a queue this tab has never been told about is `null`,
   * which is different from a queue known to be empty. */
  queued: QueuedMessage[] | null;
  /** Whether a live `InboxChanged` has arrived. Until one has, the detail
   * endpoint is the better authority and may re-seed — a session whose inbox
   * drained *before* this view subscribed broadcast its `InboxChanged` to
   * nobody, and seeding once would leave the drained message on screen for
   * good. Same hazard the `lastError` seed below exists for. */
  sawLiveInbox: boolean;
  streaming: string;
  liveStatus: SessionStatusKind | null;
  livePendingAsks: PendingAskView[] | null;
  statusSeq: number;
  statusReason: string | null;
  streamError: string | null;
  connected: boolean;
  /** A live status frame has arrived, so the session document must no longer
   * seed `streamError` over what the stream itself has said. Cleared on
   * `reset`. */
  errorLive: boolean;
  tasks: TaskItem[];
  /** A live `TaskListChanged` has arrived for this session, so the agent
   * document must no longer seed over it. Cleared on `reset`. */
  tasksLive: boolean;
  hasMoreBefore: boolean;
  loadingMore: boolean;
  progression: { stage: string; detail: string | null } | null;
  /** Bumped on every `Resync` frame — the signal to backfill from the cursor,
   * since the server deliberately does not replay a live stream. */
  needsResync: number;
  /** Bumped when the agent roster changes, so the session document is re-read. */
  agentTreeSeq: number;
}

const INITIAL: State = {
  order: [],
  byId: {},
  toolResults: {},
  liveTools: {},
  hooks: {},
  notices: {},
  hookEntryIds: {},
  optimistic: [],
  queued: null,
  sawLiveInbox: false,
  streaming: "",
  liveStatus: null,
  livePendingAsks: null,
  statusSeq: 0,
  statusReason: null,
  streamError: null,
  connected: false,
  errorLive: false,
  tasks: [],
  tasksLive: false,
  hasMoreBefore: false,
  loadingMore: false,
  progression: null,
  needsResync: 0,
  agentTreeSeq: 0,
};

type Action =
  | { kind: "reset" }
  | { kind: "connected"; value: boolean }
  | { kind: "optimistic"; id: string; text: string }
  | { kind: "remove-optimistic"; id: string }
  // The send was acknowledged: this echo now has a server-side identity.
  | { kind: "ack-optimistic"; id: string; serverId: string }
  // The queue as the *detail* endpoint reported it; ignored once a live frame
  // has arrived, which is always fresher.
  | { kind: "seed-queue"; queued: QueuedMessage[] }
  | { kind: "seed-tasks"; tasks: TaskItem[] }
  // The failed turn's reason as the *detail* endpoint reported it. A session's
  // first turn starts with the session, so its `Error` frame can be broadcast
  // before this view is subscribed and never be heard.
  | { kind: "seed-error"; error: string | null }
  | { kind: "loading-more"; value: boolean }
  | {
      kind: "history";
      page: HistoryPage;
      prepend: boolean;
      /** A forward backfill page (from `after=`), not a scroll-back or seed. */
      forward?: boolean;
    }
  | { kind: "event"; event: SessionEvent | AgentStreamEvent };

function textOf(parts: ContentPart[]): string {
  return parts
    .filter((p): p is Extract<ContentPart, { type: "Text" }> => p.type === "Text")
    .map((p) => p.value.text)
    .join("");
}

function thinkingOf(parts: ContentPart[]): string[] {
  return parts
    .filter(
      (p): p is Extract<ContentPart, { type: "Thinking" }> =>
        p.type === "Thinking",
    )
    .map((p) => p.value.text);
}

function toolCallsOf(parts: ContentPart[]) {
  return parts
    .filter(
      (p): p is Extract<ContentPart, { type: "ToolCall" }> =>
        p.type === "ToolCall",
    )
    .map((p) => ({ id: p.value.id, name: p.value.name, input: p.value.input }));
}

/** A finished subagent's report. It rides a user message on the wire — the
 * providers require that — but it is the agent's own work landing, not
 * something the person said, and the transcript renders it as such. */
function subAgentResultsOf(parts: ContentPart[]): RenderedSubAgent[] {
  return parts
    .filter(
      (p): p is Extract<ContentPart, { type: "SubAgentResult" }> =>
        p.type === "SubAgentResult",
    )
    .map((p) => ({
      subagentId: p.value.subagentId,
      label: p.value.label,
      status: p.value.status,
      text: p.value.text,
      spawnedAtMs: p.value.spawnedAtMs,
      endedAtMs: p.value.endedAtMs,
    }));
}

/** Fold one message's non-order state (byId, tool results) into the maps. */
function storeMessage(
  msg: Message,
  byId: Record<string, StoredMessage>,
  toolResults: Record<string, ToolResultEntry>,
  liveTools: Record<string, { name: string; running: boolean }>,
): void {
  if (msg.role === Role.Tool) {
    for (const part of msg.parts) {
      if (part.type === "ToolResult") {
        toolResults[part.value.toolCallId] = {
          output: part.value.output,
          isError: part.value.isError,
          atMs: msg.createdAtMs,
        };
        if (liveTools[part.value.toolCallId]) {
          liveTools[part.value.toolCallId] = {
            ...liveTools[part.value.toolCallId],
            running: false,
          };
        }
      }
    }
    return;
  }
  byId[msg.id] = {
    id: msg.id,
    role: msg.role === Role.Assistant ? "Assistant" : "User",
    text: textOf(msg.parts),
    thinking: thinkingOf(msg.parts),
    toolCalls: toolCallsOf(msg.parts),
    subagentResults: subAgentResultsOf(msg.parts),
    createdAtMs: msg.createdAtMs,
    startedAtMs: msg.startedAtMs,
  };
}

/** Apply a batch of history messages, appending or prepending fresh ids in the
 * batch's own (chronological) order and deduping against what's loaded. */
/** The LLM messages of a transcript window. A page carries entries, and not
 * every entry is a message the model saw — a hook record is an entry too. */
function llmMessages(entries: HistoryEntry[]): Message[] {
  return entries.flatMap((e) => (e.type === "Llm" ? [e.value] : []));
}

/** Route hook entries by whether they name a tool call.
 *
 * A record with a `ToolScope` attaches to that call's card; one without — a
 * `SessionStart` bootstrap, a `Stop` that kept the turn going — becomes a
 * transcript row of its own.
 *
 * Both halves dedupe against one ledger of entry ids, because a backfill can
 * overlap what the live stream already delivered. The id is the server's
 * (`hook:{n}`), derived from the journal, so the two sources agree on it.
 */
function withHookEntries(
  state: Pick<State, "hooks" | "notices" | "order" | "hookEntryIds">,
  entries: HistoryEntry[],
  prepend: boolean,
): Pick<State, "hooks" | "notices" | "order" | "hookEntryIds"> {
  const hooks = { ...state.hooks };
  const notices = { ...state.notices };
  const hookEntryIds = { ...state.hookEntryIds };
  const fresh: string[] = [];
  for (const e of entries) {
    if (e.type !== "Hook") continue;
    const entry = e.value;
    if (hookEntryIds[entry.id]) continue;
    hookEntryIds[entry.id] = true;
    const scope = toolScope(entry.record);
    if (scope) {
      hooks[scope.toolCallId] = [
        ...(hooks[scope.toolCallId] ?? []),
        entry.record,
      ];
    } else {
      notices[entry.id] = {
        id: entry.id,
        record: entry.record,
        atMs: entry.createdAtMs,
      };
      fresh.push(entry.id);
    }
  }
  const order = prepend
    ? [...fresh, ...state.order]
    : [...state.order, ...fresh];
  return { hooks, notices, order, hookEntryIds };
}

function applyHistory(state: State, messages: Message[], prepend: boolean): State {
  const byId = { ...state.byId };
  const toolResults = { ...state.toolResults };
  const liveTools = { ...state.liveTools };
  const seen = new Set(state.order);
  const fresh: string[] = [];
  for (const msg of messages) {
    storeMessage(msg, byId, toolResults, liveTools);
    if (msg.role !== Role.Tool && !seen.has(msg.id)) {
      seen.add(msg.id);
      fresh.push(msg.id);
    }
  }
  const order = prepend
    ? [...fresh, ...state.order]
    : [...state.order, ...fresh];
  return { ...state, byId, toolResults, liveTools, order };
}

function ingestMessage(state: State, msg: Message): State {
  const byId = { ...state.byId };
  const toolResults = { ...state.toolResults };
  const liveTools = { ...state.liveTools };
  const exists = state.byId[msg.id] !== undefined || msg.role === Role.Tool;
  storeMessage(msg, byId, toolResults, liveTools);

  const next: State = {
    ...state,
    byId,
    toolResults,
    liveTools,
    order:
      msg.role === Role.Tool || exists ? state.order : [...state.order, msg.id],
  };
  if (msg.role === Role.Assistant) next.streaming = "";
  if (msg.role === Role.User && state.optimistic.length > 0) {
    next.optimistic = state.optimistic.slice(1);
  }
  return next;
}

function reducer(state: State, action: Action): State {
  switch (action.kind) {
    case "reset":
      return INITIAL;
    case "connected":
      return { ...state, connected: action.value };
    case "loading-more":
      return { ...state, loadingMore: action.value };
    case "optimistic":
      return {
        ...state,
        optimistic: [...state.optimistic, { id: action.id, text: action.text }],
        // The user is starting a new turn — the last one's error is history.
        streamError: null,
      };
    case "remove-optimistic":
      return {
        ...state,
        optimistic: state.optimistic.filter((o) => o.id !== action.id),
      };
    case "ack-optimistic": {
      // Already in the queue → the server's own copy is what we render, and
      // this echo would double it.
      if (state.queued?.some((q) => q.id === action.serverId)) {
        return {
          ...state,
          optimistic: state.optimistic.filter((o) => o.id !== action.id),
        };
      }
      return {
        ...state,
        optimistic: state.optimistic.map((o) =>
          o.id === action.id ? { ...o, serverId: action.serverId } : o,
        ),
      };
    }
    case "seed-queue":
      return state.sawLiveInbox ? state : { ...state, queued: action.queued };
    // The durable task list, from the agent document. Same guard shape as
    // `seed-queue`: a live frame is always fresher, so once one has arrived
    // this is a no-op. Without it, a session the server had offloaded came
    // back from a reload with an empty plan — the list existed, but only in
    // events that had already been broadcast and would never replay.
    case "seed-tasks":
      return state.tasksLive ? state : { ...state, tasks: action.tasks };
    // Same guard shape again, and the same reason #100 gave for not showing a
    // server-held error forever: once the stream has said anything about this
    // session's status, the stream owns the banner and a stale `lastError`
    // must not reappear underneath it.
    case "seed-error":
      return state.errorLive ? state : { ...state, streamError: action.error };
    case "history": {
      const { page, prepend } = action;
      // A page carries messages and nothing else. Task list and usage are
      // current values on the agent document: usage is read straight off it
      // by the view, and the task list is seeded from it below. A page no
      // longer means two different things depending on its cursor.
      const next = applyHistory(state, llmMessages(page.entries), prepend);
      return {
        ...next,
        ...withHookEntries(next, page.entries, prepend),
        // A forward (backfill) page says nothing about what precedes the
        // window already loaded, so it must not overwrite that.
        hasMoreBefore: prepend || !action.forward ? page.hasMoreBefore : state.hasMoreBefore,
        loadingMore: false,
      };
    }
    case "event": {
      const ev = action.event;
      switch (ev.type) {
        case "Appended":
          // Real output began → the prep stage is done. A tool result is an
          // append like any other, so this one case covers the whole
          // transcript — the same fold `/history` feeds.
          return ev.value.entry.type === "Llm"
            ? { ...ingestMessage(state, ev.value.entry.value), progression: null }
            : {
                ...state,
                ...withHookEntries(state, [ev.value.entry], false),
                progression: null,
              };
        case "Resync":
          // The stream dropped frames. Say so; the effect below re-reads the
          // documents and backfills from the cursor rather than guessing.
          //
          // `tasksLive` is released here too. It exists to stop the agent
          // document overwriting a fresher live frame, but a Resync means live
          // frames were *lost* — so the document is now the better source
          // again. Left latched, the plan stayed frozen at the last delivered
          // frame until the user navigated away and back, while usage (read
          // straight off the document) recovered on its own.
          return {
            ...state,
            needsResync: state.needsResync + 1,
            tasksLive: false,
            // Released for the same reason: a lost `Error` frame is exactly
            // what the session document can still answer for.
            errorLive: false,
          };
        case "Progressed":
          return {
            ...state,
            progression: {
              stage: ev.value.stage,
              detail: ev.value.detail ?? null,
            },
          };
        case "InboxChanged": {
          const queued = ev.value.queued;
          const ids = new Set(queued.map((q) => q.id));
          return {
            ...state,
            queued,
            sawLiveInbox: true,
            // A queued message the server now owns is rendered from the queue;
            // dropping the echo here is also what keeps several messages
            // merged into one turn from leaving orphan echoes behind, since
            // that turn produces a single user message for all of them.
            optimistic: state.optimistic.filter(
              (o) => !(o.serverId && ids.has(o.serverId)),
            ),
          };
        }
        case "ToolStart":
          return {
            ...state,
            liveTools: {
              ...state.liveTools,
              [ev.value.toolCallId]: { name: ev.value.name, running: true },
            },
          };
        case "TurnCompleted":
          // Usage is deliberately not accumulated here. It is a cumulative
          // value the server owns on the agent document, and `StatusChanged`
          // below re-reads that document — so summing frames locally bought
          // nothing except a total that reset to zero on every reload,
          // because a session with no live events to replay had never seen
          // a frame to sum.
          return { ...state, streaming: "", progression: null };
        case "StatusChanged":
          return {
            ...state,
            liveStatus: ev.value.status,
            livePendingAsks: ev.value.pendingAsks,
            statusSeq: state.statusSeq + 1,
            statusReason: ev.value.reason ?? null,
            // Latched by a turn *starting*, not by any status frame at all. A
            // turn that begins is what makes a server-held error stale; an
            // `Idle` frame says nothing about whether the turn before it
            // failed, and latching on it threw away the seeded banner for the
            // failure that had just happened.
            errorLive:
              state.errorLive || ev.value.status === SessionStatusKind.Running,
            // A turn that has started supersedes the previous turn's error.
            // The optimistic echo already clears it for a message sent from
            // this view; this also covers turns with no echo of their own —
            // an answer to a pending ask, or a message sent from another tab.
            // Safe against ordering: the server reports Running at turn start,
            // strictly before any `Error` frame that turn can produce.
            streamError:
              ev.value.status === SessionStatusKind.Running
                ? null
                : state.streamError,
          };
        case "Error":
          return { ...state, streamError: ev.value.message, errorLive: true };
        case "Delta":
          return {
            ...state,
            streaming: state.streaming + ev.value.text,
            progression: null,
          };
        case "TaskListChanged":
          // `tasksLive` latches: from here the stream is the fresher source
          // and the agent document must not overwrite it. The document is
          // only re-read on `StatusChanged`, so mid-turn it is already stale.
          return { ...state, tasks: ev.value.tasks, tasksLive: true };
        case "AgentTreeChanged":
          return { ...state, agentTreeSeq: state.agentTreeSeq + 1 };
        default:
          return state;
      }
    }
    default:
      return state;
  }
}

let optimisticSeq = 0;

/**
 * Loads a session's transcript as a *window* of the latest messages via
 * `GET /history` (task list + usage ride the tail page), then subscribes to a
 * live-only SSE stream for new events. Scroll-back pages are pulled on demand
 * with `loadMore`. Live events that arrive before the tail is seeded are
 * buffered and replayed after, so ordering stays correct without a gap.
 */
export function useSessionStream(
  sessionId: string | undefined,
  /** Which agent's transcript to follow. Defaults to the session's main agent;
   * a workflow step or a subagent passes its own id. The session-scoped stream
   * (status, inbox, roster) is the same either way. */
  agentId: string = MAIN_AGENT,
): {
  stream: SessionStream;
  addOptimisticUser: (text: string) => string;
  removeOptimisticUser: (id: string) => void;
  ackOptimisticUser: (id: string, serverId: string) => void;
  loadMore: () => void;
} {
  const [state, dispatch] = useReducer(reducer, INITIAL);
  const queryClient = useQueryClient();
  // The durable queue, for a session opened with messages already waiting.
  // Shares `useSession`'s cache entry with the view, so this costs no request.
  const { data: detail } = useSession(sessionId);
  // Shares `SessionView`'s cache entry, so this is a read of state already
  // fetched rather than a second request.
  const { data: mainAgent } = useAgent(sessionId, agentId);
  const esRef = useRef<EventSource | null>(null);
  // Earliest loaded message id — the cursor for the next scroll-back page.
  const earliestRef = useRef<string | null>(null);
  earliestRef.current = state.order[0] ?? null;
  const canLoadMore = state.hasMoreBefore && !state.loadingMore;
  const canLoadMoreRef = useRef(canLoadMore);
  canLoadMoreRef.current = canLoadMore;

  useEffect(() => {
    dispatch({ kind: "reset" });
    if (!sessionId) return;

    let cancelled = false;
    let seeded = false;
    const buffer: (SessionEvent | AgentStreamEvent)[] = [];

    // Re-read a current value when a frame says it changed. Nothing here
    // accumulates: every one of these is a document the server owns, and the
    // frame is only a signal to fetch it again.
    //
    // Usage is refreshed on status change rather than `TurnCompleted`: the
    // server broadcasts `TurnCompleted` from inside the agent's own run —
    // before the session actor has processed the durable usage push that
    // `handle_finished` sends *after* that (`UsageRecorded` then
    // `Concluded`/`Asked`, delivered to the same mailbox in that order).
    // `StatusChanged` only fires once `Concluded`/`Asked`/`Failed` runs, so by
    // then the session's own usage total has landed.
    const refreshDocuments = (event: SessionEvent | AgentStreamEvent) => {
      switch (event.type) {
        case "StatusChanged":
          void queryClient.invalidateQueries({
            queryKey: qk.agent(sessionId, agentId),
          });
          void queryClient.invalidateQueries({
            queryKey: qk.session(sessionId),
          });
          break;
        case "AgentTreeChanged":
          void queryClient.invalidateQueries({
            queryKey: qk.session(sessionId),
          });
          break;
        default:
          break;
      }
    };

    const ingest = (event: SessionEvent | AgentStreamEvent) => {
      if (seeded) {
        dispatch({ kind: "event", event });
        refreshDocuments(event);
      } else buffer.push(event);
    };

    const open = (
      url: string,
      label: string,
      settled?: () => void,
    ): EventSource => {
      const es = new EventSource(url);
      es.onopen = () => {
        dispatch({ kind: "connected", value: true });
        settled?.();
      };
      es.onmessage = (e: MessageEvent<string>) => {
        try {
          ingest(JSON.parse(e.data) as SessionEvent | AgentStreamEvent);
        } catch (err) {
          console.error(`failed to parse ${label} event`, err, e.data);
        }
      };
      es.onerror = () => {
        dispatch({ kind: "connected", value: false });
        // A stream that will not open must not strand the seed below: the
        // transcript is worth showing even when nothing live can reach it.
        settled?.();
      };
      return es;
    };

    // Two streams, matching the two scopes. The session stream carries status,
    // inbox, progression and roster changes; the agent stream carries this
    // agent's transcript and its live run frames. The browser resumes the
    // agent stream on its own via `Last-Event-ID` (the last appended message
    // id), which the server serves from the agent's state.
    // Each stream catches up on the state *it* carries, and each waits to be
    // connected before doing it — not merely constructed. A session's first
    // turn now starts with the session itself, since the create carries the
    // first message, so a whole turn can begin and end while the browser is
    // still navigating. Reading before the subscription is live leaves a hole
    // exactly the width of the connect: whatever happens inside it is in
    // neither the read nor the stream.
    //
    // Which is why the two catch-ups are wired to different streams. The
    // transcript rides the agent stream; status, asks and the queue ride the
    // session stream. Hanging both off one of them left the other's gap open —
    // a turn whose completion frame was broadcast before the session stream
    // connected left the badge reading `Running` with nothing left to correct
    // it.
    let historyStarted = false;
    const seedHistory = () => {
      if (historyStarted || cancelled) return;
      historyStarted = true;
      const flush = () => {
        if (cancelled) return;
        seeded = true;
        for (const event of buffer) {
          dispatch({ kind: "event", event });
          refreshDocuments(event);
        }
        buffer.length = 0;
      };
      api.sessions
        .history(sessionId, agentId, { limit: HISTORY_LIMIT })
        .then((page) => {
          if (cancelled) return;
          dispatch({ kind: "history", page, prepend: false });
          flush();
        })
        // Let live events flow even if the initial fetch failed.
        .catch(flush);
    };

    let documentsStarted = false;
    const seedDocuments = () => {
      if (documentsStarted || cancelled) return;
      documentsStarted = true;
      // `refetch`, not `invalidate`: this has to read the server even when the
      // view's own fetch is still in flight or has already settled, because
      // what it corrects is a frame nobody was listening for.
      void queryClient.refetchQueries({ queryKey: qk.session(sessionId) });
      void queryClient.refetchQueries({
        queryKey: qk.agent(sessionId, agentId),
      });
    };

    const sessionEs = open(
      api.sessionEventsUrl(sessionId),
      "session",
      seedDocuments,
    );
    const agentEs = open(
      api.agentEventsUrl(sessionId, agentId),
      "agent",
      seedHistory,
    );
    esRef.current = agentEs;

    return () => {
      cancelled = true;
      sessionEs.close();
      agentEs.close();
      esRef.current = null;
    };
  }, [sessionId, queryClient]);

  // A `Resync` means the stream dropped frames. Backfill forward from the
  // newest message we hold and re-read the documents; the server deliberately
  // does not replay, because a live stream is not a log.
  const latestRef = useRef<string | null>(null);
  latestRef.current = state.order[state.order.length - 1] ?? null;
  const needsResync = state.needsResync;
  useEffect(() => {
    if (!sessionId || needsResync === 0) return;
    const after = latestRef.current;
    void queryClient.invalidateQueries({ queryKey: qk.session(sessionId) });
    void queryClient.invalidateQueries({
      queryKey: qk.agent(sessionId, agentId),
    });
    if (!after) return;
    api.sessions
      .history(sessionId, agentId, { after, limit: HISTORY_LIMIT })
      .then((page) =>
        dispatch({ kind: "history", page, prepend: false, forward: true }),
      )
      .catch(() => {});
  }, [sessionId, needsResync, queryClient]);

  const seedQueue = detail?.inbox;
  useEffect(() => {
    if (seedQueue) dispatch({ kind: "seed-queue", queued: seedQueue });
  }, [seedQueue]);

  // The last turn's failure, from the session document. A turn that failed
  // before this view subscribed — which a session's first turn now can, since
  // the create starts it — broadcast its `Error` frame to nobody, and the
  // banner is the only thing that says the turn did not simply end.
  const seedError = detail?.lastError ?? null;
  const sessionKey = detail?.id;
  useEffect(() => {
    if (sessionKey) dispatch({ kind: "seed-error", error: seedError });
  }, [sessionKey, seedError]);

  // The main agent's durable `task_list` state. `useAgent` shares its cache
  // entry with the view, so this costs no extra request, and `StatusChanged`
  // already invalidates it. The reducer ignores this once a live frame has
  // landed.
  const seedTasks = mainAgent?.tasks;
  useEffect(() => {
    if (seedTasks) dispatch({ kind: "seed-tasks", tasks: seedTasks });
  }, [seedTasks]);

  const loadMore = useCallback(() => {
    const before = earliestRef.current;
    if (!sessionId || !before || !canLoadMoreRef.current) return;
    dispatch({ kind: "loading-more", value: true });
    api.sessions
      .history(sessionId, agentId, { before, limit: HISTORY_LIMIT })
      .then((page) => dispatch({ kind: "history", page, prepend: true }))
      .catch(() => dispatch({ kind: "loading-more", value: false }));
  }, [sessionId]);

  const addOptimisticUser = (text: string) => {
    const id = `optim-${optimisticSeq++}`;
    dispatch({ kind: "optimistic", id, text });
    return id;
  };

  const removeOptimisticUser = (id: string) => {
    dispatch({ kind: "remove-optimistic", id });
  };

  const ackOptimisticUser = (id: string, serverId: string) => {
    dispatch({ kind: "ack-optimistic", id, serverId });
  };

  // A call in the transcript with no result yet, in a session that is running,
  // *is* running. Derived rather than taken from the `ToolStart` frame alone:
  // that frame is broadcast once, so a client that subscribed after it — a
  // reload mid-turn, or the first turn of a session, which now starts with the
  // session itself — never heard it and drew a finished-looking row over a call
  // still in flight. The transcript and the status both survive; between them
  // they say the same thing the frame said.
  const sessionRunning =
    (state.liveStatus ?? detail?.status) === SessionStatusKind.Running;

  const stream = useMemo<SessionStream>(() => {
    const resolveTool = (tc: {
      id: string;
      name: string;
      input: unknown;
    }): RenderedToolCall => {
      const result = state.toolResults[tc.id];
      const live = state.liveTools[tc.id];
      return {
        ...tc,
        output: result?.output,
        isError: result?.isError,
        endedAtMs: result?.atMs,
        running: result === undefined && (live?.running ?? sessionRunning),
        hooks: state.hooks[tc.id] ?? [],
      };
    };

    // One pass over `order`, which holds message ids and notice ids alike, so
    // a notice sits exactly where the journal put it rather than being appended
    // to the end of the conversation.
    const items: TranscriptItem[] = state.order.map((id) => {
      const notice = state.notices[id];
      if (notice) return { kind: "notice", value: notice };
      const m = state.byId[id];
      return {
        kind: "message",
        value: { ...m, toolCalls: m.toolCalls.map(resolveTool) },
      };
    });

    // Queued first, then this tab's un-acknowledged echoes: everything the
    // server already holds is older than anything still in flight to it.
    for (const q of state.queued ?? []) {
      items.push({
        kind: "message",
        value: {
          id: q.id,
          role: "User",
          text: q.text,
          thinking: [],
          toolCalls: [],
          subagentResults: [],
          queued: true,
        },
      });
    }

    for (const opt of state.optimistic) {
      items.push({
        kind: "message",
        value: {
          id: opt.id,
          role: "User",
          text: opt.text,
          thinking: [],
          toolCalls: [],
          subagentResults: [],
          optimistic: true,
        },
      });
    }

    // Tools that started before their assistant message finalized.
    const known = new Set<string>();
    for (const id of state.order)
      for (const tc of state.byId[id]?.toolCalls ?? []) known.add(tc.id);
    const orphanTools: RenderedToolCall[] = Object.entries(state.liveTools)
      .filter(([id]) => !known.has(id))
      .map(([id, t]) => ({
        id,
        name: t.name,
        input: undefined,
        hooks: state.hooks[id] ?? [],
        output: state.toolResults[id]?.output,
        isError: state.toolResults[id]?.isError,
        endedAtMs: state.toolResults[id]?.atMs,
        running: state.toolResults[id] === undefined && t.running,
      }));

    return {
      items,
      streaming: state.streaming,
      orphanTools,
      liveStatus: state.liveStatus,
      livePendingAsks: state.livePendingAsks,
      statusSeq: state.statusSeq,
      statusReason: state.statusReason,
      streamError: state.streamError,
      connected: state.connected,
      tasks: state.tasks,
      hasMoreBefore: state.hasMoreBefore,
      loadingMore: state.loadingMore,
      progression: state.progression,
    };
  }, [state, sessionRunning]);

  return {
    stream,
    addOptimisticUser,
    removeOptimisticUser,
    ackOptimisticUser,
    loadMore,
  };
}
