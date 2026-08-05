import { ShieldAlert, ShieldCheck } from "lucide-react";
import type { HookRecord } from "../api/types";
import { cn } from "../lib/cn";
import { hookSummary, systemMessage } from "../lib/hookSummary";

/** A hook record with no tool call of its own — a `SessionStart` bootstrap, a
 * `Stop` that kept the turn going.
 *
 * It has nowhere to attach, so it is a row. Deliberately quieter than a tool
 * card: this is something a plugin did *around* the conversation, not something
 * the agent asked for. */
export function HookNoticeRow({ record }: { record: HookRecord }) {
  const { text, intervened } = hookSummary(record);
  const note = systemMessage(record);
  return (
    <div
      data-testid="hook-notice"
      data-event={record.action.event}
      data-intervened={intervened ? "true" : "false"}
      className="flex items-start gap-2 py-1"
    >
      <span className="flex w-3.5 shrink-0 justify-center pt-0.5">
        {intervened ? (
          <ShieldAlert size={12} className="text-amber-ink" aria-hidden />
        ) : (
          <ShieldCheck size={12} className="text-faint" aria-hidden />
        )}
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="shrink-0 font-mono text-[0.6875rem] font-medium tracking-[0.02em] text-legend">
            {record.plugin}
          </span>
          <span className="legend shrink-0">{record.action.event}</span>
          <span
            className={cn(
              "min-w-0 flex-1 truncate font-mono text-[0.6875rem]",
              intervened ? "text-dim" : "text-faint",
            )}
          >
            {text}
          </span>
        </div>
        {/* `systemMessage` is addressed to the user, never the model. It has
            been captured and shown to nobody since #140; this is where it
            lands. */}
        {note && (
          <p
            data-testid="hook-notice-system-message"
            className="mt-0.5 text-[0.6875rem] leading-relaxed text-amber-ink"
          >
            {note}
          </p>
        )}
      </div>
    </div>
  );
}
