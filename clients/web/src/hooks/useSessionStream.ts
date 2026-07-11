import { useEffect, useMemo, useReducer, useRef } from "react";
import { api } from "../api/client";
import {
  Role,
  SessionStatusKind,
  type ContentPart,
  type Message,
  type SessionEvent,
} from "../api/types";

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
};

type Action =
  | { kind: "reset" }
  | { kind: "connected"; value: boolean }
  | { kind: "optimistic"; id: string; text: string }
  | { kind: "event"; event: SessionEvent };

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

function ingestMessage(state: State, msg: Message): State {
  // Tool-role messages carry only ToolResult parts; fold them into the result
  // map so they render inside the originating assistant's tool-call card.
  if (msg.role === Role.Tool) {
    const toolResults = { ...state.toolResults };
    const liveTools = { ...state.liveTools };
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
    return { ...state, toolResults, liveTools };
  }

  const role = msg.role === Role.Assistant ? "Assistant" : "User";
  const stored: StoredMessage = {
    id: msg.id,
    role,
    text: textOf(msg.parts),
    thinking: thinkingOf(msg.parts),
    toolCalls: toolCallsOf(msg.parts),
  };

  const exists = state.byId[msg.id] !== undefined;
  const next: State = {
    ...state,
    byId: { ...state.byId, [msg.id]: stored },
    order: exists ? state.order : [...state.order, msg.id],
  };

  // A finalized assistant message supersedes the live streaming buffer.
  if (role === "Assistant") next.streaming = "";
  // A real user message confirms the oldest optimistic echo.
  if (role === "User" && state.optimistic.length > 0) {
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
    case "optimistic":
      return {
        ...state,
        optimistic: [
          ...state.optimistic,
          { id: action.id, text: action.text },
        ],
      };
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
            usage: {
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
 * Subscribes to a session's SSE stream and folds durable history + live frames
 * into a render-ready transcript. EventSource replays journalled events on
 * connect and auto-resumes with `Last-Event-ID` on reconnect, so the fold is
 * dedup-safe by message id.
 */
export function useSessionStream(sessionId: string | undefined): {
  stream: SessionStream;
  addOptimisticUser: (text: string) => void;
} {
  const [state, dispatch] = useReducer(reducer, INITIAL);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    dispatch({ kind: "reset" });
    if (!sessionId) return;

    const es = new EventSource(api.sessionEventsUrl(sessionId));
    esRef.current = es;

    es.onopen = () => dispatch({ kind: "connected", value: true });
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        const event = JSON.parse(e.data) as SessionEvent;
        dispatch({ kind: "event", event });
      } catch (err) {
        console.error("failed to parse session event", err, e.data);
      }
    };
    es.onerror = () => dispatch({ kind: "connected", value: false });

    return () => {
      es.close();
      esRef.current = null;
    };
  }, [sessionId]);

  const addOptimisticUser = (text: string) => {
    dispatch({ kind: "optimistic", id: `optim-${optimisticSeq++}`, text });
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
    };
  }, [state]);

  return { stream, addOptimisticUser };
}
