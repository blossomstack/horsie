import { useRef } from "react";
import type { HookRecord } from "../api/types";
import { cn } from "../lib/cn";
import type {
  RenderedMessage,
  RenderedToolCall,
  TranscriptItem,
} from "../hooks/useSessionStream";
import { buildSegments, type Segment } from "../lib/transcriptSegments";
import { HookNoticeRow } from "./HookNoticeRow";
import { Prose } from "./Prose";
import { ToolCallCard } from "./ToolCallCard";
import { TurnActions } from "./TurnActions";
import { WorkGroup } from "./WorkGroup";

/**
 * One entry in the recording.
 *
 * `group` + `relative` exist for `TurnActions`, which floats into the 1.75rem
 * gap above the turn rather than reserving a strip of its own: at ~24px on
 * every entry, a permanent control row would cost more vertical space than
 * the channel gutter it replaced.
 */
function Turn({
  className,
  children,
  actions,
  ...rest
}: {
  className?: string;
  children: React.ReactNode;
  actions?: React.ReactNode;
} & React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("animate-settle group relative min-w-0", className)}
      {...rest}
    >
      {actions}
      <div className="min-w-0 space-y-2">{children}</div>
    </div>
  );
}

function SegmentView({
  segment,
  showThinking,
}: {
  segment: Segment;
  showThinking: boolean;
}) {
  switch (segment.kind) {
    case "text":
      return (
        <div
          // Marks the prose of a turn, as distinct from its tool traffic.
          // `TurnActions` reads only these when copying plain text — reading
          // the whole turn swept in tool-call names, work-group summaries and
          // ask-card choices, so the two copy buttons returned different
          // content from each other.
          data-prose-segment=""
          data-testid={
            segment.streaming ? "assistant-streaming" : "assistant-text"
          }
        >
          <Prose text={segment.text} />
        </div>
      );
    case "work":
      return (
        <WorkGroup
          items={segment.items}
          live={segment.live}
          showThinking={showThinking}
          startedAtMs={segment.startedAtMs}
          endedAtMs={segment.endedAtMs}
        />
      );
    case "ask":
      return <ToolCallCard call={segment.call} />;
    case "pulse":
      return (
        <div className="pt-0.5" data-testid="pulse">
          <span className="caret" aria-label="The agent is working" />
        </div>
      );
  }
}

/** A run of consecutive assistant messages (no interleaved user turn) is one
 * channel entry — an agent's multi-step tool-call trajectory is one continuous
 * thread of work, not a series of separate replies. `live` merges a
 * still-streaming tail into the same entry when it continues this turn; with
 * empty `msgs` it renders a turn that is entirely live. */
function AssistantTurn({
  msgs,
  live,
  showThinking,
}: {
  msgs: RenderedMessage[];
  live?: { text: string; orphanTools: RenderedToolCall[] };
  showThinking: boolean;
}) {
  const segments = buildSegments(msgs, live);
  const bodyRef = useRef<HTMLDivElement>(null);

  // What a copy takes: the prose the user reads, not the tool traffic
  // underneath it. A turn still in flight offers no copy — its text is
  // arriving, so anything taken now is a fragment of a sentence.
  const markdown = live
    ? undefined
    : segments
        .filter(
          (s): s is Extract<Segment, { kind: "text" }> => s.kind === "text",
        )
        .map((s) => s.text)
        .join("\n\n")
        .trim();
  const atMs = live ? undefined : msgs[msgs.length - 1]?.createdAtMs;

  return (
    <Turn
      data-testid="message"
      data-role="Assistant"
      actions={
        markdown ? (
          <TurnActions atMs={atMs} markdown={markdown} renderedRef={bodyRef} />
        ) : undefined
      }
    >
      <div ref={bodyRef} className="min-w-0 space-y-2">
        {segments.length === 0 ? (
          <span className="caret" aria-label="The agent is working" />
        ) : (
          segments.map((s) => (
            <SegmentView key={s.key} segment={s} showThinking={showThinking} />
          ))
        )}
      </div>
    </Turn>
  );
}

function UserTurn({ msg }: { msg: RenderedMessage }) {
  // One copy button, not two: a user message is plain text already, so a
  // markdown flavour would be a second control with the same outcome.
  const settled = !msg.optimistic && !msg.queued;
  return (
    <Turn
      className={cn((msg.optimistic || msg.queued) && "opacity-60")}
      data-testid="message"
      data-role={msg.role}
      data-queued={msg.queued ? "true" : undefined}
      actions={
        settled ? (
          <TurnActions atMs={msg.createdAtMs} plainText={msg.text} />
        ) : undefined
      }
    >
      <div className="rounded-[var(--radius-control)] bg-raised px-3.5 py-2.5 shadow-[inset_0_0_0_1px_var(--row-ring)] text-[0.9375rem] leading-relaxed break-words whitespace-pre-wrap text-legend">
        {msg.text}
      </div>
      {msg.queued && (
        <div className="legend" data-testid="queued-marker">
          Unsent — goes in with the next turn
        </div>
      )}
    </Turn>
  );
}

/** Consecutive assistant messages collapse into one entry; user messages
 * always start a fresh one. */
export type TurnGroup =
  | { kind: "user"; msg: RenderedMessage }
  | { kind: "assistant"; id: string; msgs: RenderedMessage[] }
  // Never folded into an assistant turn: a plugin acting *around* the
  // conversation is not something the agent said.
  | { kind: "notice"; id: string; record: HookRecord };

export function groupTurns(items: TranscriptItem[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
  const intoAssistant = (m: RenderedMessage) => {
    const last = turns[turns.length - 1];
    if (last?.kind === "assistant") last.msgs.push(m);
    else turns.push({ kind: "assistant", id: m.id, msgs: [m] });
  };
  for (const item of items) {
    if (item.kind === "notice") {
      turns.push({
        kind: "notice",
        id: item.value.id,
        record: item.value.record,
      });
      continue;
    }
    const m = item.value;
    if (m.role === "User") {
      // A subagent's result rides a user message because the providers demand
      // it, but it is the agent's own work landing — not something the person
      // said. It joins the assistant thread, and only what was actually typed
      // gets a bubble. A turn carrying results alone gets no bubble at all.
      if (m.subagentResults.length > 0) {
        intoAssistant({
          ...m,
          id: `${m.id}:sub`,
          text: "",
          thinking: [],
          toolCalls: [],
        });
      }
      if (m.text) turns.push({ kind: "user", msg: m });
      continue;
    }
    intoAssistant(m);
  }
  return turns;
}

/** Which turn the live tail continues, or `null` when it starts a turn of its
 * own below everything.
 *
 * The running turn is the last one built from messages the server has actually
 * consumed — so trailing *pending* bubbles are skipped, and a consumed message
 * stops the search. Both halves matter and they pull in opposite directions:
 *
 * - A message sent while the agent works is queued and drawn after the turn it
 *   will follow. Treating the last turn as live handed the tail to that bubble
 *   and left the running work group on its past-tense summary — "Ran 2 tools"
 *   over a tool with five seconds left.
 * - A message the server has consumed is part of the conversation, and what the
 *   agent says next belongs *after* it. Treating the last assistant turn as
 *   live folded a new turn's output into the previous one, drawing the answer
 *   above the message that asked for it.
 *
 * Position was only ever a proxy for "the turn that is running". This is the
 * thing itself: pending messages are not part of the conversation yet, so they
 * cannot end it. */
export function liveTurnIndex(turns: TurnGroup[]): number | null {
  for (let i = turns.length - 1; i >= 0; i--) {
    const turn = turns[i];
    if (turn.kind === "assistant") return i;
    if (turn.kind === "user" && !turn.msg.queued && !turn.msg.optimistic) {
      return null;
    }
  }
  return null;
}

export function Transcript({
  items,
  streaming,
  orphanTools,
  showLive,
  showThinking,
}: {
  items: TranscriptItem[];
  streaming: string;
  orphanTools: RenderedToolCall[];
  showLive: boolean;
  showThinking: boolean;
}) {
  const turns = groupTurns(items);
  // Gated on session status alone (not on whether content has arrived yet) so
  // the live tail — and its caret — is reachable during the gap between
  // "Running" and the first token or tool.
  const hasLive = showLive;
  // The turn the live tail belongs to; `null` means it has nowhere to attach
  // and stands on its own below the conversation.
  const liveIndex = hasLive ? liveTurnIndex(turns) : null;

  return (
    <div className="mx-auto flex w-full max-w-[54rem] flex-col gap-7 px-4 py-7 sm:px-6">
      {turns.map((t, i) =>
        t.kind === "notice" ? (
          <HookNoticeRow key={t.id} record={t.record} />
        ) : t.kind === "user" ? (
          <UserTurn key={t.msg.id} msg={t.msg} />
        ) : (
          <AssistantTurn
            key={t.id}
            msgs={t.msgs}
            showThinking={showThinking}
            live={
              liveIndex === i ? { text: streaming, orphanTools } : undefined
            }
          />
        ),
      )}
      {hasLive && liveIndex === null && (
        <AssistantTurn
          key="streaming"
          msgs={[]}
          showThinking={showThinking}
          live={{ text: streaming, orphanTools }}
        />
      )}
    </div>
  );
}
