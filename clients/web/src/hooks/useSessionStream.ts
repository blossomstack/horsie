import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import { api } from "../api/client";
import {
  Role,
  SessionStatusKind,
  type ContentPart,
  type HistoryPage,
  type Message,
  type SessionEvent,
  type TaskItem,
} from "../api/types";

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
}

export interface RenderedMessage {
  id: string;
  role: "User" | "Assistant";
  text: string;
  thinking: string[];
  toolCalls: RenderedToolCall[];
  optimistic?: boolean;
}

export interface SessionStream {
  messages: RenderedMessage[];
  /** Live, not-yet-finalized assistant text (from Delta events). */
  streaming: string;
  /** Tools started but not yet attached to a finalized assistant message. */
  orphanTools: RenderedToolCall[];
  usage: { input: number; output: number };
  liveStatus: SessionStatusKind | null;
  statusReason: string | null;
  pendingQuestion: string | null;
  streamError: string | null;
  connected: boolean;
  /** The agent's `task_list` tool state; empty until the tool is first used. */
  tasks: TaskItem[];
  /** Older messages exist before the currently-loaded window. */
  hasMoreBefore: boolean;
  /** A scroll-back page load is in flight. */
  loadingMore: boolean;
}

// ---- Normalized reducer state ----------------------------------------------

interface StoredMessage {
  id: string;
  role: "User" | "Assistant";
  text: string;
  thinking: string[];
  toolCalls: { id: string; name: string; input: unknown }[];
}

interface State {
  order: string[];
  byId: Record<string, StoredMessage>;
  toolResults: Record<string, { output: string; isError: boolean }>;
  liveTools: Record<string, { name: string; running: boolean }>;
  optimistic: { id: string; text: string }[];
  streaming: string;
  usage: { input: number; output: number };
  liveStatus: SessionStatusKind | null;
  statusReason: string | null;
  pendingQuestion: string | null;
  streamError: string | null;
  connected: boolean;
  tasks: TaskItem[];
  hasMoreBefore: boolean;
  loadingMore: boolean;
}

const INITIAL: State = {
  order: [],
  byId: {},
  toolResults: {},
  liveTools: {},
  optimistic: [],
  streaming: "",
  usage: { input: 0, output: 0 },
  liveStatus: null,
  statusReason: null,
  pendingQuestion: null,
  streamError: null,
  connected: false,
  tasks: [],
  hasMoreBefore: false,
  loadingMore: false,
};

type Action =
  | { kind: "reset" }
  | { kind: "connected"; value: boolean }
  | { kind: "optimistic"; id: string; text: string }
  | { kind: "remove-optimistic"; id: string }
  | { kind: "loading-more"; value: boolean }
  | { kind: "history"; page: HistoryPage; prepend: boolean }
  // `fromBackfill` marks a live event replayed from the pre-seed buffer: its
  // turn's usage is already in the seeded tail total, so it must not re-add.
  | { kind: "event"; event: SessionEvent; fromBackfill?: boolean };

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

/** Fold one message's non-order state (byId, tool results) into the maps. */
function storeMessage(
  msg: Message,
  byId: Record<string, StoredMessage>,
  toolResults: Record<string, { output: string; isError: boolean }>,
  liveTools: Record<string, { name: string; running: boolean }>,
): void {
  if (msg.role === Role.Tool) {
    for (const part of msg.parts) {
      if (part.type === "ToolResult") {
        toolResults[part.value.toolCallId] = {
          output: part.value.output,
          isError: part.value.isError,
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
  };
}

/** Apply a batch of history messages, appending or prepending fresh ids in the
 * batch's own (chronological) order and deduping against what's loaded. */
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
      };
    case "remove-optimistic":
      return {
        ...state,
        optimistic: state.optimistic.filter((o) => o.id !== action.id),
      };
    case "history": {
      const { page, prepend } = action;
      let next = applyHistory(state, page.messages, prepend);
      next = { ...next, hasMoreBefore: page.hasMore, loadingMore: false };
      // Tasks + usage ride only the tail page: seed them absolutely.
      if (page.tasks) next.tasks = page.tasks;
      if (page.usage) {
        next.usage = {
          input: Number(page.usage.inputTokens),
          output: Number(page.usage.outputTokens),
        };
      }
      return next;
    }
    case "event": {
      const ev = action.event;
      switch (ev.type) {
        case "Message":
          return ingestMessage(state, ev.value.message);
        case "ToolResult": {
          const liveTools = { ...state.liveTools };
          if (liveTools[ev.value.toolCallId]) {
            liveTools[ev.value.toolCallId] = {
              ...liveTools[ev.value.toolCallId],
              running: false,
            };
          }
          return {
            ...state,
            liveTools,
            toolResults: {
              ...state.toolResults,
              [ev.value.toolCallId]: {
                output: ev.value.output,
                isError: ev.value.isError,
              },
            },
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
          return {
            ...state,
            streaming: "",
            // A backfilled turn's usage is already in the seeded tail total.
            usage: action.fromBackfill
              ? state.usage
              : {
                  input: state.usage.input + ev.value.usage.inputTokens,
                  output: state.usage.output + ev.value.usage.outputTokens,
                },
          };
        case "Asked":
          return { ...state, pendingQuestion: ev.value.question };
        case "StatusChanged":
          return {
            ...state,
            liveStatus: ev.value.status,
            statusReason: ev.value.reason ?? null,
            pendingQuestion:
              ev.value.status === SessionStatusKind.AwaitingInput
                ? state.pendingQuestion
                : null,
          };
        case "Error":
          return { ...state, streamError: ev.value.message };
        case "Delta":
          return { ...state, streaming: state.streaming + ev.value.text };
        case "TaskListChanged":
          return { ...state, tasks: ev.value.tasks };
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
export function useSessionStream(sessionId: string | undefined): {
  stream: SessionStream;
  addOptimisticUser: (text: string) => string;
  removeOptimisticUser: (id: string) => void;
  loadMore: () => void;
} {
  const [state, dispatch] = useReducer(reducer, INITIAL);
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
    const buffer: SessionEvent[] = [];

    // Live-only SSE: events before the tail seed are buffered, then replayed.
    const es = new EventSource(api.sessionEventsUrl(sessionId, { live: true }));
    esRef.current = es;
    es.onopen = () => dispatch({ kind: "connected", value: true });
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        const event = JSON.parse(e.data) as SessionEvent;
        if (seeded) dispatch({ kind: "event", event });
        else buffer.push(event);
      } catch (err) {
        console.error("failed to parse session event", err, e.data);
      }
    };
    es.onerror = () => dispatch({ kind: "connected", value: false });

    // Seed the latest window, then flush anything buffered during the fetch.
    api.sessions
      .history(sessionId, { limit: HISTORY_LIMIT })
      .then((page) => {
        if (cancelled) return;
        dispatch({ kind: "history", page, prepend: false });
        seeded = true;
        for (const event of buffer)
          dispatch({ kind: "event", event, fromBackfill: true });
        buffer.length = 0;
      })
      .catch(() => {
        if (cancelled) return;
        // Let live events flow even if the initial fetch failed.
        seeded = true;
        for (const event of buffer) dispatch({ kind: "event", event });
        buffer.length = 0;
      });

    return () => {
      cancelled = true;
      es.close();
      esRef.current = null;
    };
  }, [sessionId]);

  const loadMore = useCallback(() => {
    const before = earliestRef.current;
    if (!sessionId || !before || !canLoadMoreRef.current) return;
    dispatch({ kind: "loading-more", value: true });
    api.sessions
      .history(sessionId, { before, limit: HISTORY_LIMIT })
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
        running: result === undefined && (live?.running ?? false),
      };
    };

    const messages: RenderedMessage[] = state.order.map((id) => {
      const m = state.byId[id];
      return { ...m, toolCalls: m.toolCalls.map(resolveTool) };
    });

    for (const opt of state.optimistic) {
      messages.push({
        id: opt.id,
        role: "User",
        text: opt.text,
        thinking: [],
        toolCalls: [],
        optimistic: true,
      });
    }

    // Tools that started before their assistant message finalized.
    const known = new Set<string>();
    for (const m of state.order)
      for (const tc of state.byId[m].toolCalls) known.add(tc.id);
    const orphanTools: RenderedToolCall[] = Object.entries(state.liveTools)
      .filter(([id]) => !known.has(id))
      .map(([id, t]) => ({
        id,
        name: t.name,
        input: undefined,
        output: state.toolResults[id]?.output,
        isError: state.toolResults[id]?.isError,
        running: state.toolResults[id] === undefined && t.running,
      }));

    return {
      messages,
      streaming: state.streaming,
      orphanTools,
      usage: state.usage,
      liveStatus: state.liveStatus,
      statusReason: state.statusReason,
      pendingQuestion: state.pendingQuestion,
      streamError: state.streamError,
      connected: state.connected,
      tasks: state.tasks,
      hasMoreBefore: state.hasMoreBefore,
      loadingMore: state.loadingMore,
    };
  }, [state]);

  return { stream, addOptimisticUser, removeOptimisticUser, loadMore };
}
