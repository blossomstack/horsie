import { cn } from "../lib/cn";
import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";
import { buildSegments, type Segment } from "../lib/transcriptSegments";
import { Prose } from "./Prose";
import { ToolCallCard } from "./ToolCallCard";
import { WorkGroup } from "./WorkGroup";

function AssistantAvatar() {
  return (
    <div
      className="flex h-7 w-7 shrink-0 select-none items-center justify-center rounded-lg text-sm font-bold text-accent-fg"
      style={{ background: "var(--accent)" }}
      aria-hidden
    >
      h
    </div>
  );
}

function UserBubble({ text }: { text: string }) {
  return (
    <div className="flex justify-end">
      <div
        className="max-w-[85%] rounded-[var(--radius-lg)] rounded-br-sm px-3.5 py-2.5 text-[0.9375rem] leading-relaxed whitespace-pre-wrap text-text"
        style={{ background: "var(--surface-3)" }}
      >
        {text}
      </div>
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
        <div data-testid={segment.streaming ? "assistant-streaming" : "assistant-text"}>
          <Prose text={segment.text} />
        </div>
      );
    case "work":
      return (
        <WorkGroup items={segment.items} live={segment.live} showThinking={showThinking} />
      );
    case "ask":
      return <ToolCallCard call={segment.call} />;
    case "pulse":
      return (
        <div className="flex items-center gap-1.5 pt-1 text-sm text-faint" data-testid="pulse">
          <span className="cursor-dot" />
        </div>
      );
  }
}

/** A run of consecutive assistant messages (no interleaved user turn) shares
 * one avatar — an agent's multi-step tool-call trajectory is one continuous
 * thread of work, not a series of separate replies. `live` merges a
 * still-streaming tail into the same avatar when it continues this turn;
 * with empty `msgs` it renders a turn that is entirely live. */
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
  return (
    <div data-testid="message" data-role="Assistant" className="flex gap-3 animate-rise">
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2 pt-0.5">
        {segments.length === 0 ? (
          <span className="text-sm text-faint">…</span>
        ) : (
          segments.map((s) => (
            <SegmentView key={s.key} segment={s} showThinking={showThinking} />
          ))
        )}
      </div>
    </div>
  );
}

function UserTurn({ msg }: { msg: RenderedMessage }) {
  return (
    <div
      data-testid="message"
      data-role={msg.role}
      className={cn("animate-rise", msg.optimistic && "opacity-70")}
    >
      <UserBubble text={msg.text} />
    </div>
  );
}

/** Consecutive assistant messages collapse into one AssistantTurn; user
 * messages always start a fresh turn. */
type Turn =
  | { kind: "user"; msg: RenderedMessage }
  | { kind: "assistant"; id: string; msgs: RenderedMessage[] };

function groupTurns(messages: RenderedMessage[]): Turn[] {
  const turns: Turn[] = [];
  for (const m of messages) {
    if (m.role === "User") {
      turns.push({ kind: "user", msg: m });
      continue;
    }
    const last = turns[turns.length - 1];
    if (last?.kind === "assistant") last.msgs.push(m);
    else turns.push({ kind: "assistant", id: m.id, msgs: [m] });
  }
  return turns;
}

export function Transcript({
  messages,
  streaming,
  orphanTools,
  showLive,
  showThinking,
}: {
  messages: RenderedMessage[];
  streaming: string;
  orphanTools: RenderedToolCall[];
  showLive: boolean;
  showThinking: boolean;
}) {
  const turns = groupTurns(messages);
  // Gated on session status alone (not on whether content has arrived yet)
  // so the live tail — and its `pulse` progress indicator — is reachable
  // during the gap between "Running" and the first token/tool.
  const hasLive = showLive;
  const lastTurn = turns[turns.length - 1];
  // A live tail with no interleaved user message continues the last turn —
  // merge it into that turn's avatar instead of popping in a new one.
  const mergeLiveIntoLastTurn = hasLive && lastTurn?.kind === "assistant";

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-5 px-4 py-6">
      {turns.map((t, i) =>
        t.kind === "user" ? (
          <UserTurn key={t.msg.id} msg={t.msg} />
        ) : (
          <AssistantTurn
            key={t.id}
            msgs={t.msgs}
            showThinking={showThinking}
            live={
              mergeLiveIntoLastTurn && i === turns.length - 1
                ? { text: streaming, orphanTools }
                : undefined
            }
          />
        ),
      )}
      {hasLive && !mergeLiveIntoLastTurn && (
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
