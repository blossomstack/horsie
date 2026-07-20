import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";

/** The server's dedicated "ask the user" tool for sessions — kept in sync
 * with the same constant in ToolCallCard.tsx. */
const ASK_USER_TOOL = "ask_user";

export type WorkItem =
  | { kind: "thinking"; text: string }
  | { kind: "tool"; call: RenderedToolCall };

export type Segment =
  | { kind: "text"; key: string; text: string; streaming?: boolean }
  | { kind: "work"; key: string; items: WorkItem[]; live: boolean }
  | { kind: "ask"; key: string; call: RenderedToolCall }
  | { kind: "pulse"; key: string };

/**
 * Flattens a turn's messages (+ optional live tail) into a linear sequence
 * of text / grouped-work / standalone-question / pulse segments.
 *
 * Consecutive thinking blocks and regular tool calls — across message
 * (LLM-iteration) boundaries, as long as no text or `ask_user` call
 * interrupts them — collapse into one `work` segment. `ask_user` always
 * breaks the run and renders standalone: a pending question must never be
 * hidden inside a collapsed group.
 */
export function buildSegments(
  msgs: RenderedMessage[],
  live?: { text: string; orphanTools: RenderedToolCall[] },
): Segment[] {
  const segments: Segment[] = [];
  let work: WorkItem[] = [];
  let seq = 0;

  const flushWork = (isLive: boolean) => {
    if (work.length > 0) {
      segments.push({ kind: "work", key: `work${seq++}`, items: work, live: isLive });
      work = [];
    }
  };

  const pushToolCall = (call: RenderedToolCall) => {
    if (call.name === ASK_USER_TOOL) {
      flushWork(false);
      segments.push({ kind: "ask", key: `ask${seq++}`, call });
    } else {
      work.push({ kind: "tool", call });
    }
  };

  for (const m of msgs) {
    for (const t of m.thinking) work.push({ kind: "thinking", text: t });
    if (m.text) {
      flushWork(false);
      segments.push({ kind: "text", key: `text${seq++}`, text: m.text });
    }
    for (const tc of m.toolCalls) pushToolCall(tc);
  }

  if (live) {
    for (const tc of live.orphanTools) pushToolCall(tc);
    if (live.text) {
      flushWork(false);
      segments.push({ kind: "text", key: `text${seq++}`, text: live.text, streaming: true });
    }
    if (work.length > 0) flushWork(true);
    // Only pulse when the turn has produced nothing at all yet — not after
    // e.g. a finalized text answer, which can still be the merge target for
    // one more render (streaming reset to "", status not yet Idle).
    else if (!live.text && segments.length === 0) {
      segments.push({ kind: "pulse", key: `pulse${seq++}` });
    }
  } else {
    flushWork(false);
  }

  return segments;
}
