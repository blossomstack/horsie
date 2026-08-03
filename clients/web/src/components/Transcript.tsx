import { cn } from "../lib/cn";
import type {
  RenderedMessage,
  RenderedToolCall,
} from "../hooks/useSessionStream";
import { buildSegments, type Segment } from "../lib/transcriptSegments";
import { Prose } from "./Prose";
import { ToolCallCard } from "./ToolCallCard";
import { WorkGroup } from "./WorkGroup";
import { formatTime } from "../lib/time";

/** The channel gutter. Every turn is stamped and labelled in the same
 * left-hand column, so a long recording reads down one edge instead of
 * zig-zagging between bubbles. Times come only from the server's own stamps —
 * an optimistic echo has none, and inventing one from the local clock would
 * misreport when the turn actually happened. */
function Gutter({ channel, atMs }: { channel: string; atMs?: number }) {
  return (
    <div className="flex shrink-0 items-baseline gap-2 sm:w-[4.75rem] sm:flex-col sm:items-end sm:gap-0.5">
      <span className="legend !text-faint">{channel}</span>
      {atMs !== undefined && (
        <span
          className="legend tabular-nums"
          data-testid="turn-time"
          title={new Date(atMs).toLocaleString()}
        >
          {formatTime(atMs)}
        </span>
      )}
    </div>
  );
}

function Turn({
  channel,
  atMs,
  className,
  children,
  ...rest
}: {
  channel: string;
  atMs?: number;
  className?: string;
  children: React.ReactNode;
} & React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("animate-settle flex flex-col gap-1 sm:flex-row sm:gap-4", className)}
      {...rest}
    >
      <Gutter channel={channel} atMs={atMs} />
      <div className="min-w-0 flex-1 space-y-2">{children}</div>
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
  return (
    <Turn
      channel="Agent"
      atMs={live ? undefined : msgs[msgs.length - 1]?.createdAtMs}
      data-testid="message"
      data-role="Assistant"
    >
      {segments.length === 0 ? (
        <span className="caret" aria-label="The agent is working" />
      ) : (
        segments.map((s) => (
          <SegmentView key={s.key} segment={s} showThinking={showThinking} />
        ))
      )}
    </Turn>
  );
}

function UserTurn({ msg }: { msg: RenderedMessage }) {
  return (
    <Turn
      channel="You"
      atMs={msg.queued ? undefined : msg.createdAtMs}
      className={cn((msg.optimistic || msg.queued) && "opacity-60")}
      data-testid="message"
      data-role={msg.role}
      data-queued={msg.queued ? "true" : undefined}
    >
      <div className="rounded-[var(--radius-control)] border bg-raised px-3.5 py-2.5 text-[0.9375rem] leading-relaxed whitespace-pre-wrap text-legend">
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
type TurnGroup =
  | { kind: "user"; msg: RenderedMessage }
  | { kind: "assistant"; id: string; msgs: RenderedMessage[] };

function groupTurns(messages: RenderedMessage[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
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
  // Gated on session status alone (not on whether content has arrived yet) so
  // the live tail — and its caret — is reachable during the gap between
  // "Running" and the first token or tool.
  const hasLive = showLive;
  const lastTurn = turns[turns.length - 1];
  // A live tail with no interleaved user message continues the last entry.
  const mergeLiveIntoLastTurn = hasLive && lastTurn?.kind === "assistant";

  return (
    <div className="mx-auto flex w-full max-w-[54rem] flex-col gap-7 px-4 py-7 sm:px-6">
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
