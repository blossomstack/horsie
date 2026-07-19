import { cn } from "../lib/cn";
import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";
import { Prose } from "./Prose";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallCard } from "./ToolCallCard";

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

/** One agent iteration's content (thinking / text / tool calls), unadorned —
 * the enclosing turn supplies the avatar and spacing. */
function AssistantStep({ msg }: { msg: RenderedMessage }) {
  return (
    <div data-testid="message" data-role={msg.role} className="space-y-2">
      {msg.thinking.map((t, i) => (
        <ThinkingBlock key={`t${i}`} text={t} />
      ))}
      {msg.text && (
        <div data-testid="assistant-text">
          <Prose text={msg.text} />
        </div>
      )}
      {msg.toolCalls.map((tc) => (
        <ToolCallCard key={tc.id} call={tc} />
      ))}
    </div>
  );
}

/** The live, not-yet-finalized tail of a turn: streaming text and/or tool
 * calls still running. Shares its parent turn's avatar. */
function LiveTail({
  text,
  orphanTools,
}: {
  text: string;
  orphanTools: RenderedToolCall[];
}) {
  return (
    <>
      {text ? (
        <div data-testid="assistant-streaming">
          <Prose text={text} />
        </div>
      ) : orphanTools.length === 0 ? (
        <div className="flex items-center gap-1.5 pt-1 text-sm text-faint">
          <span className="cursor-dot" />
        </div>
      ) : null}
      {orphanTools.map((tc) => (
        <ToolCallCard key={tc.id} call={tc} />
      ))}
    </>
  );
}

/** A run of consecutive assistant messages (no interleaved user turn) shares
 * one avatar — an agent's multi-step tool-call trajectory is one continuous
 * thread of work, not a series of separate replies. `live` merges a
 * still-streaming tail into the same avatar when it continues this turn. */
function AssistantTurn({
  msgs,
  live,
}: {
  msgs: RenderedMessage[];
  live?: { text: string; orphanTools: RenderedToolCall[] };
}) {
  const hasBody =
    msgs.some((m) => m.text.length > 0 || m.thinking.length > 0 || m.toolCalls.length > 0) ||
    !!live;
  return (
    <div className={cn("flex gap-3 animate-rise", msgs[0]?.optimistic && "opacity-70")}>
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2 pt-0.5">
        {msgs.map((m) => (
          <AssistantStep key={m.id} msg={m} />
        ))}
        {live && <LiveTail text={live.text} orphanTools={live.orphanTools} />}
        {!hasBody && <span className="text-sm text-faint">…</span>}
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

function StreamingTurn({
  text,
  orphanTools,
}: {
  text: string;
  orphanTools: RenderedToolCall[];
}) {
  return (
    <div className="flex gap-3">
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2 pt-0.5">
        <LiveTail text={text} orphanTools={orphanTools} />
      </div>
    </div>
  );
}

export function Transcript({
  messages,
  streaming,
  orphanTools,
  showLive,
}: {
  messages: RenderedMessage[];
  streaming: string;
  orphanTools: RenderedToolCall[];
  showLive: boolean;
}) {
  const turns = groupTurns(messages);
  const hasLive = showLive && (streaming.length > 0 || orphanTools.length > 0);
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
            live={
              mergeLiveIntoLastTurn && i === turns.length - 1
                ? { text: streaming, orphanTools }
                : undefined
            }
          />
        ),
      )}
      {hasLive && !mergeLiveIntoLastTurn && (
        <StreamingTurn text={streaming} orphanTools={orphanTools} />
      )}
    </div>
  );
}
