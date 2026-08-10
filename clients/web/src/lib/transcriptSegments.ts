import type {
  RenderedMessage,
  RenderedSubAgent,
  RenderedToolCall,
} from "../hooks/useSessionStream";
import { isAskCall } from "./askUser";

export type WorkItem =
  | { kind: "thinking"; text: string }
  | { kind: "tool"; call: RenderedToolCall }
  | { kind: "subagent"; result: RenderedSubAgent };

export type Segment =
  | { kind: "text"; key: string; text: string; streaming?: boolean }
  | {
      kind: "work";
      key: string;
      items: WorkItem[];
      live: boolean;
      /** Server-stamped span of the work in this group, when known: from the
       * earliest provider call that produced it to the last tool that
       * answered. Absent for a group made only of live, not-yet-finalized
       * items, which have no server stamps yet. */
      startedAtMs?: number;
      endedAtMs?: number;
    }
  | { kind: "ask"; key: string; call: RenderedToolCall }
  | { kind: "pulse"; key: string };

/**
 * Flattens a turn's messages (+ optional live tail) into a linear sequence
 * of text / grouped-work / standalone-question / pulse segments.
 *
 * Consecutive thinking blocks and regular tool calls — across message
 * (LLM-iteration) boundaries, as long as no text or question interrupts
 * them — collapse into one `work` segment. A question always breaks the run
 * and renders standalone: a pending question must never be hidden inside a
 * collapsed group, and an answered one is the record that a human was asked.
 *
 * What counts as a question is `isAskCall`, not the `ask_user` name: a
 * workflow step has no `ask_user` and asks through `conclude` instead.
 */
export function buildSegments(
  msgs: RenderedMessage[],
  live?: { text: string; orphanTools: RenderedToolCall[] },
): Segment[] {
  const segments: Segment[] = [];
  let work: WorkItem[] = [];
  let seq = 0;
  let workStart: number | undefined;
  let workEnd: number | undefined;

  const extend = (start?: number, end?: number) => {
    if (start !== undefined) workStart = Math.min(workStart ?? start, start);
    if (end !== undefined) workEnd = Math.max(workEnd ?? end, end);
  };

  const flushWork = (isLive: boolean) => {
    if (work.length > 0) {
      segments.push({
        kind: "work",
        key: `work${seq++}`,
        items: work,
        live: isLive,
        startedAtMs: workStart,
        endedAtMs: workEnd,
      });
      work = [];
    }
    workStart = undefined;
    workEnd = undefined;
  };

  const pushToolCall = (call: RenderedToolCall) => {
    if (isAskCall(call.name, call.input)) {
      flushWork(false);
      segments.push({ kind: "ask", key: `ask${seq++}`, call });
    } else {
      work.push({ kind: "tool", call });
      extend(undefined, call.endedAtMs);
    }
  };

  for (const m of msgs) {
    // A subagent carries its own span — it ran outside this turn entirely, so
    // the message's stamps say nothing about how long the work took.
    for (const r of m.subagentResults) {
      work.push({ kind: "subagent", result: r });
      if (r.spawnedAtMs > 0 && r.endedAtMs > 0) extend(r.spawnedAtMs, r.endedAtMs);
    }
    // The message's own span bounds whatever it contributed: thinking happened
    // during the provider call, and its tool calls were issued at the end of it.
    const contributes = m.thinking.length > 0 || m.toolCalls.length > 0;
    if (contributes) extend(m.startedAtMs ?? m.createdAtMs, m.createdAtMs);
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
