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

function AssistantMessage({ msg }: { msg: RenderedMessage }) {
  const hasBody =
    msg.text.length > 0 || msg.thinking.length > 0 || msg.toolCalls.length > 0;
  return (
    <div className="flex gap-3">
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2.5 pt-0.5">
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
        {!hasBody && <span className="text-sm text-faint">…</span>}
      </div>
    </div>
  );
}

function MessageRow({ msg }: { msg: RenderedMessage }) {
  return (
    <div
      data-testid="message"
      data-role={msg.role}
      className={cn("animate-rise", msg.optimistic && "opacity-70")}
    >
      {msg.role === "User" ? (
        <UserBubble text={msg.text} />
      ) : (
        <AssistantMessage msg={msg} />
      )}
    </div>
  );
}

function StreamingMessage({
  text,
  orphanTools,
}: {
  text: string;
  orphanTools: RenderedToolCall[];
}) {
  return (
    <div className="flex gap-3">
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2.5 pt-0.5">
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
  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-5 px-4 py-6">
      {messages.map((m) => (
        <MessageRow key={m.id} msg={m} />
      ))}
      {showLive && (streaming.length > 0 || orphanTools.length > 0) && (
        <StreamingMessage text={streaming} orphanTools={orphanTools} />
      )}
    </div>
  );
}
