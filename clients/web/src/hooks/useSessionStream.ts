import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import { MAIN_AGENT, api } from "../api/client";
import {
  Role,
  SessionStatusKind,
  type AgentLogEntry,
  type ContentPart,
  type HookRecord,
  type Message,
  type MessageFrame,
  type MessagesPage,
  type AskLifecycle,
  type TaskItem,
} from "../api/types";
import { toolScope } from "../lib/hookSummary";
import { qk, useAgent, useSession } from "./useSessions";

/** Entries per scroll-back page. */
const PAGE = 50;

// ---- View model handed to the UI -------------------------------------------

export interface RenderedToolCall {
  id: string;
  name: string;
  input: unknown;
  output?: string;
  isError?: boolean;
  running: boolean;
  endedAtMs?: number;
  hooks: HookRecord[];
}

export interface RenderedSubAgent {
  subagentId: string;
  label: string;
  status: string;
  text: string;
  spawnedAtMs: number;
  endedAtMs: number;
}

export interface RenderedMessage {
  id: string;
  role: "User" | "Assistant";
  text: string;
  thinking: string[];
  toolCalls: RenderedToolCall[];
  subagentResults: RenderedSubAgent[];
  createdAtMs?: number;
  startedAtMs?: number;
  optimistic?: boolean;
  queued?: boolean;
}

export interface RenderedHookNotice {
  id: string;
  record: HookRecord;
  atMs: number;
}

export type TranscriptItem =
  | { kind: "message"; value: RenderedMessage }
  | { kind: "notice"; value: RenderedHookNotice };

export interface SessionStream {
  items: TranscriptItem[];
  /** Live, not-yet-finalized assistant text. */
  streaming: string;
  /** Always empty now. A tool call is visible the moment its assistant message
   * lands, and there is no separate `ToolStart` frame that could arrive first —
   * so there is no such thing as a tool with no message to hang it on. Kept as
   * a field because the transcript's segment builder takes one. */
  orphanTools: RenderedToolCall[];
  liveStatus: SessionStatusKind | null;
  livePendingAsks: AskLifecycle[] | null;
  statusSeq: number;
  statusReason: string | null;
  streamError: string | null;
  connected: boolean;
  tasks: TaskItem[];
  hasMoreBefore: boolean;
  loadingMore: boolean;
  progression: { stage: string; detail: string | null } | null;
}

// ---- Reducer ---------------------------------------------------------------

interface State {
  /** The log, in seq order. The single source for everything below it. */
  entries: AgentLogEntry[];
  /** Chunks of the message being written, since the newest entry. */
  deltas: string[];
  /** Local echoes of messages this tab sent, shown until the server's own
   * account of them arrives. `serverId` is what the send was acknowledged
   * with, which is also how the echo learns it can go. */
  optimistic: { id: string; text: string; serverId?: string }[];
  connected: boolean;
  /** Whether older entries exist before the oldest one held.
   *
   * Told by the server's window frame rather than inferred from the first
   * entry's `seq`: a cursorless connect replays a capped window, and a log
   * front-trimmed for context management would also start above zero — the two
   * look identical from here and want opposite answers. */
  hasMoreBefore: boolean;
  loadingMore: boolean;
  /** The agent document's task list, and the log position it reflects.
   *
   * Two sources for one fact, made safe by being comparable: a value applies
   * only if its seq is greater than the seq the current value came from. That
   * is what a boolean latch could never do — it cannot tell a document that is
   * *ahead* of the fold from one that is behind, which is why the old one
   * needed a lost-frame signal to release it. */
  docTasks: { tasks: TaskItem[]; asOfSeq: number } | null;
}

const INITIAL: State = {
  entries: [],
  deltas: [],
  optimistic: [],
  connected: false,
  hasMoreBefore: false,
  loadingMore: false,
  docTasks: null,
};

type Action =
  | { kind: "reset" }
  | { kind: "connected"; value: boolean }
  | { kind: "frame"; frame: MessageFrame }
  | { kind: "prepend"; page: MessagesPage }
  | { kind: "loading-more"; value: boolean }
  | { kind: "optimistic"; id: string; text: string }
  | { kind: "remove-optimistic"; id: string }
  | { kind: "ack-optimistic"; id: string; serverId: string }
  | { kind: "doc-tasks"; tasks: TaskItem[]; asOfSeq: number };

/** Insert `incoming` into `entries`, keeping seq order and dropping duplicates.
 *
 * A duplicate is possible only across a reconnect that resends the frame the
 * client's cursor named; ordering is otherwise guaranteed by the server, which
 * has one writer. Cheap because the common case is "append at the end". */
function merge(entries: AgentLogEntry[], incoming: AgentLogEntry): AgentLogEntry[] {
  const last = entries[entries.length - 1];
  if (!last || incoming.seq > last.seq) return [...entries, incoming];
  const at = entries.findIndex((e) => e.seq >= incoming.seq);
  if (entries[at]?.seq === incoming.seq) return entries;
  return [...entries.slice(0, at), incoming, ...entries.slice(at)];
}

function reducer(state: State, action: Action): State {
  switch (action.kind) {
    case "reset":
      return INITIAL;
    case "connected":
      return { ...state, connected: action.value };
    case "loading-more":
      return { ...state, loadingMore: action.value };
    case "frame": {
      const frame = action.frame;
      if (frame.type === "Entry") {
        const entry = frame.value;
        const queuedIds = new Set(
          entry.body.type === "Lifecycle" &&
            entry.body.value.kind === "MessageQueued"
            ? [entry.body.value.value.id]
            : [],
        );
        return {
          ...state,
          entries: merge(state.entries, entry),
          // An entry supersedes the chunks that preceded it.
          deltas: [],
          // The server now owns this message, so the local echo is redundant.
          optimistic: state.optimistic.filter(
            (o) => !(o.serverId && queuedIds.has(o.serverId)),
          ),
        };
      }
      if (frame.type === "Window") {
        return { ...state, hasMoreBefore: frame.value.hasMoreBefore };
      }
      // A reset means the run that produced the chunks we hold is gone.
      return {
        ...state,
        deltas: frame.value.reset
          ? [frame.value.text]
          : [...state.deltas, frame.value.text],
      };
    }
    case "prepend": {
      const merged = action.page.entries.reduce(merge, state.entries);
      return {
        ...state,
        entries: merged,
        // Fewer than asked for means we reached the start. On a page that is
        // the whole signal — unlike the replay, where the cap is not the
        // client's own limit and so has to be stated.
        hasMoreBefore: action.page.entries.length >= PAGE,
        loadingMore: false,
      };
    }
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
    case "ack-optimistic":
      return {
        ...state,
        optimistic: state.optimistic.map((o) =>
          o.id === action.id ? { ...o, serverId: action.serverId } : o,
        ),
      };
    case "doc-tasks":
      return {
        ...state,
        docTasks: { tasks: action.tasks, asOfSeq: action.asOfSeq },
      };
    default:
      return state;
  }
}

// ---- The fold --------------------------------------------------------------

interface Folded {
  status: SessionStatusKind | null;
  statusSeq: number;
  reason: string | null;
  error: string | null;
  pendingAsks: AskLifecycle[];
  queued: { id: string; text: string }[];
  tasks: TaskItem[] | null;
  /** Seq of the entry the task list came from, for comparing against a
   * document read. */
  tasksSeq: number;
  progression: { stage: string; detail: string | null } | null;
}

/** Everything the session used to report out-of-band, computed from the log.
 *
 * This is the duplication the design accepts: the server folds the same events
 * into the same values, and the two must agree. It buys the removal of every
 * seed-and-guard pair the client used to need — a fact with one source cannot
 * race itself.
 */
export function fold(entries: AgentLogEntry[]): Folded {
  const out: Folded = {
    status: null,
    statusSeq: 0,
    reason: null,
    error: null,
    pendingAsks: [],
    queued: [],
    tasks: null,
    tasksSeq: -1,
    progression: null,
  };
  for (const entry of entries) {
    if (entry.body.type !== "Lifecycle") continue;
    const ev = entry.body.value;
    switch (ev.kind) {
      // The session's sandbox, as opposed to a turn's setup below. Two
      // variants rather than one stage string because they are two different
      // facts that happened to share the label "ready".
      case "Runtime": {
        const status = ev.value.status.kind;
        out.progression = {
          stage: `runtime_${status.toLowerCase()}`,
          detail: ev.value.detail ?? null,
        };
        if (status === "Failed") {
          out.status = SessionStatusKind.Failed;
          out.reason = ev.value.detail ?? null;
        } else if (out.status === null) {
          out.status = SessionStatusKind.Provisioning;
        }
        out.statusSeq += 1;
        break;
      }
      // This turn's setup: narration only, and never a status.
      case "Preparing":
        out.progression = {
          stage: ev.value.stage,
          detail: ev.value.detail ?? null,
        };
        break;
      case "MessageQueued":
        out.queued.push({ id: ev.value.id, text: ev.value.text });
        break;
      case "TurnBegan": {
        const consumed = new Set(ev.value.consumed);
        out.queued = out.queued.filter((q) => !consumed.has(q.id));
        // A turn beginning ends the park either way: the questions were
        // answered, or the person moved on and they were abandoned. `answered`
        // says which of the two happened; it does not decide what is still
        // pending, because after either one nothing is.
        out.pendingAsks = [];
        out.status = SessionStatusKind.Running;
        out.statusSeq += 1;
        // A turn that starts supersedes the previous turn's failure.
        out.error = null;
        out.progression = null;
        break;
      }
      case "TurnEnded": {
        const outcome = ev.value.outcome;
        out.status =
          outcome.kind === "Failed"
            ? SessionStatusKind.Failed
            : SessionStatusKind.Idle;
        out.error = outcome.kind === "Failed" ? outcome.value.error : null;
        out.reason = out.error;
        out.statusSeq += 1;
        out.progression = null;
        break;
      }
      case "AskRecorded":
        out.pendingAsks.push(ev.value);
        out.status = SessionStatusKind.AwaitingInput;
        out.statusSeq += 1;
        break;
      case "TaskList":
        out.tasks = ev.value.tasks;
        out.tasksSeq = entry.seq;
        break;
      // A subagent appearing or finishing, and a step starting or ending, are
      // both progress worth showing while a turn is in flight. They used to
      // arrive twice — once as these typed entries, which nothing read, and
      // once as a free-text `Provisioning` entry the server wrote by hand. The
      // server now writes only these, so the progress line is derived from the
      // typed fact rather than from a string built to be displayed.
      case "SubAgent":
        out.progression = {
          stage: `subagent_${ev.value.status}`,
          detail: `"${ev.value.label}" (${ev.value.id})`,
        };
        break;
      case "Step":
        out.progression = {
          stage: `step_${ev.value.status}`,
          detail: ev.value.name,
        };
        break;
      case "SessionFailed":
        out.status = SessionStatusKind.Unrecoverable;
        out.reason = ev.value.reason;
        out.error = ev.value.reason;
        out.statusSeq += 1;
        break;
      default:
        break;
    }
  }
  return out;
}

// ---- Entry → view model ----------------------------------------------------

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

let optimisticSeq = 0;

/**
 * Reads one agent's log through `GET /sessions/:id/messages`.
 *
 * One connection, one order, one source. The stream replays from the start and
 * then goes live, so there is no seam between a backfill and a subscription for
 * a turn to fall through — which is what the seed-and-guard pairs this hook
 * used to carry were all managing. Status, the queue, the task list and the
 * last error are folded from the same entries the transcript is built from.
 */
export function useSessionStream(
  sessionId: string | undefined,
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
  const { data: detail } = useSession(sessionId);
  const { data: agentDoc } = useAgent(sessionId, agentId);

  const earliestRef = useRef<number | null>(null);
  earliestRef.current = state.entries[0]?.seq ?? null;
  const canLoadMore = state.hasMoreBefore && !state.loadingMore;
  const canLoadMoreRef = useRef(canLoadMore);
  canLoadMoreRef.current = canLoadMore;

  useEffect(() => {
    dispatch({ kind: "reset" });
    if (!sessionId) return;

    const es = new EventSource(api.messagesUrl(sessionId, agentId));
    es.onopen = () => dispatch({ kind: "connected", value: true });
    es.onerror = () => dispatch({ kind: "connected", value: false });
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        dispatch({ kind: "frame", frame: JSON.parse(e.data) as MessageFrame });
      } catch (err) {
        console.error("failed to parse a messages frame", err, e.data);
      }
    };
    return () => es.close();
  }, [sessionId, agentId]);

  // Re-read the documents when a turn boundary passes. Only the numbers live
  // there now — usage, context size — plus the task list, which is compared by
  // seq rather than latched.
  const statusSeq = useMemo(() => fold(state.entries).statusSeq, [state.entries]);
  useEffect(() => {
    if (!sessionId || statusSeq === 0) return;
    void queryClient.invalidateQueries({ queryKey: qk.agent(sessionId, agentId) });
    void queryClient.invalidateQueries({ queryKey: qk.session(sessionId) });
  }, [sessionId, agentId, statusSeq, queryClient]);

  const docTasks = agentDoc?.tasks;
  const docSeq = agentDoc?.asOfSeq;
  useEffect(() => {
    if (docTasks && docSeq !== undefined) {
      dispatch({ kind: "doc-tasks", tasks: docTasks, asOfSeq: docSeq });
    }
  }, [docTasks, docSeq]);

  const loadMore = useCallback(() => {
    const before = earliestRef.current;
    if (!sessionId || before === null || !canLoadMoreRef.current) return;
    dispatch({ kind: "loading-more", value: true });
    api.sessions
      .messages(sessionId, agentId, { before, max: PAGE })
      .then((page) => dispatch({ kind: "prepend", page }))
      .catch(() => dispatch({ kind: "loading-more", value: false }));
  }, [sessionId, agentId]);

  const addOptimisticUser = (text: string) => {
    const id = `optim-${optimisticSeq++}`;
    dispatch({ kind: "optimistic", id, text });
    return id;
  };
  const removeOptimisticUser = (id: string) =>
    dispatch({ kind: "remove-optimistic", id });
  const ackOptimisticUser = (id: string, serverId: string) =>
    dispatch({ kind: "ack-optimistic", id, serverId });

  const stream = useMemo<SessionStream>(() => {
    const folded = fold(state.entries);
    const running = folded.status === SessionStatusKind.Running;

    // Tool results and hook records, keyed by the call they answer, so a call
    // can be resolved wherever it appears.
    const results = new Map<string, { output: string; isError: boolean; atMs: number }>();
    const hooks = new Map<string, HookRecord[]>();
    for (const entry of state.entries) {
      if (entry.body.type === "Llm" && entry.body.value.role === Role.Tool) {
        for (const part of entry.body.value.parts) {
          if (part.type === "ToolResult") {
            results.set(part.value.toolCallId, {
              output: part.value.output,
              isError: part.value.isError,
              atMs: entry.atMs,
            });
          }
        }
      } else if (entry.body.type === "Hook") {
        const scope = toolScope(entry.body.value.record);
        if (scope) {
          hooks.set(scope.toolCallId, [
            ...(hooks.get(scope.toolCallId) ?? []),
            entry.body.value.record,
          ]);
        }
      }
    }

    const resolveTool = (tc: { id: string; name: string; input: unknown }): RenderedToolCall => {
      const result = results.get(tc.id);
      return {
        ...tc,
        output: result?.output,
        isError: result?.isError,
        endedAtMs: result?.atMs,
        // A call with no result yet, in a session that is running, *is*
        // running. Derived rather than pushed, so a client that joined late
        // draws it the same way one that watched it start does.
        running: result === undefined && running,
        hooks: hooks.get(tc.id) ?? [],
      };
    };

    const renderMessage = (m: Message): RenderedMessage => ({
      id: m.id,
      role: m.role === Role.Assistant ? "Assistant" : "User",
      text: textOf(m.parts),
      thinking: thinkingOf(m.parts),
      toolCalls: m.parts
        .filter(
          (p): p is Extract<ContentPart, { type: "ToolCall" }> =>
            p.type === "ToolCall",
        )
        .map((p) =>
          resolveTool({ id: p.value.id, name: p.value.name, input: p.value.input }),
        ),
      subagentResults: subAgentResultsOf(m.parts),
      createdAtMs: m.createdAtMs,
      startedAtMs: m.startedAtMs ?? undefined,
    });

    const items: TranscriptItem[] = [];
    for (const entry of state.entries) {
      if (entry.body.type === "Llm") {
        if (entry.body.value.role === Role.Tool) continue;
        items.push({ kind: "message", value: renderMessage(entry.body.value) });
      } else if (entry.body.type === "Hook") {
        const record = entry.body.value.record;
        if (toolScope(record)) continue; // shown on its call's card
        items.push({
          kind: "notice",
          value: { id: entry.body.value.id, record, atMs: entry.body.value.createdAtMs },
        });
      }
      // Lifecycle entries drive the fold above; they are not transcript rows.
    }

    // Queued first, then this tab's un-acknowledged echoes: everything the
    // server already holds is older than anything still in flight to it.
    for (const q of folded.queued) {
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
    const queuedIds = new Set(folded.queued.map((q) => q.id));
    for (const opt of state.optimistic) {
      if (opt.serverId && queuedIds.has(opt.serverId)) continue;
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

    // The document wins only when it reflects a later position than the entry
    // the folded list came from.
    const tasks =
      state.docTasks && state.docTasks.asOfSeq > folded.tasksSeq
        ? state.docTasks.tasks
        : (folded.tasks ?? []);

    return {
      items,
      streaming: state.deltas.join(""),
      orphanTools: [],
      liveStatus: folded.status,
      livePendingAsks: folded.status === null ? null : folded.pendingAsks,
      statusSeq: folded.statusSeq,
      statusReason: folded.reason,
      streamError: folded.error,
      connected: state.connected,
      tasks,
      hasMoreBefore: state.hasMoreBefore,
      loadingMore: state.loadingMore,
      progression: folded.progression,
    };
  }, [state]);

  // `detail` is still read for settings the log does not carry (name, vendor,
  // repos). It is deliberately NOT a source of status, the queue, or the last
  // error any more — those come from the log, which is the only thing the two
  // could have disagreed about.
  void detail;

  return {
    stream,
    addOptimisticUser,
    removeOptimisticUser,
    ackOptimisticUser,
    loadMore,
  };
}
