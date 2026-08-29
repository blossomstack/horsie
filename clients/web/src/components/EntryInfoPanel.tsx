import { MessageSquareText } from "lucide-react";
import type { RenderedMessage } from "../hooks/useSessionStream";
import { SECTION_TITLE } from "./AgentInfoPanel";
import { absoluteTime, clockTime, humanDuration } from "../lib/format";
import Markdown from "./Markdown";
import { SidePanel } from "./SidePanel";
import { useTranslation } from "react-i18next";

/** Label on the left, figure on the right — the row `AgentInfoPanel` uses, so
 *  the two panels that answer for the same picture read the same way. */
function TimeRow({
  label,
  value,
  hint,
  testId,
}: {
  label: string;
  value: string;
  hint?: string;
  testId?: string;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-[3px]" title={hint}>
      <span className="legend">{label}</span>
      <span className="readout text-xs" data-testid={testId}>
        {value}
      </span>
    </div>
  );
}

/**
 * What one thing on the timeline actually was.
 *
 * The timeline draws durations; it cannot draw content, and a bar you cannot
 * read is a bar you have to leave the view to identify. Clicking one used to
 * switch back to the transcript and scroll — which answered the question by
 * closing the picture that raised it. This answers it beside the picture.
 *
 * Deliberately not a second transcript: one message, its text, its thinking
 * and the calls it issued. The transcript is one key away for the rest.
 */
export function EntryInfoPanel({
  message,
  onClose,
  onOpenTranscript,
}: {
  /** The message the selected bar belongs to. */
  message: RenderedMessage;
  onClose: () => void;
  /** Go and read it in place, with everything around it. */
  onOpenTranscript: (entryId: string) => void;
}) {
  const role = message.role === "User" ? "User" : "Assistant";
  // The same reading of the stamps the timeline lays out with: a message spans
  // the provider call that produced it, and each of its tool calls was issued
  // at the end of that call. A bar's length is a duration and a bar cannot say
  // what it is, so the panel beside it says it.
  const at = message.createdAtMs ?? 0;
  const { t } = useTranslation();
  const began = message.startedAtMs ?? at;
  const took = at > began ? at - began : null;
  return (
    <SidePanel
      legend={t("entryPanel.legend")}
      readout={
        <span className="readout text-[0.6875rem]" data-testid="entry-panel-readout">
          {role.toLowerCase()}
        </span>
      }
      onClose={onClose}
      closeLabel={t("entryPanel.close")}
      testId="entry-panel"
      closeTestId="entry-panel-collapse"
    >
      <div className="min-h-0 flex-1 overflow-y-auto">
        {at > 0 && (
          <section className="px-3 py-2.5">
            <h3 className={SECTION_TITLE}>{t("entryPanel.timing")}</h3>
            <div className="mt-1.5">
              <TimeRow
                label={t("entryPanel.at")}
                value={clockTime(at)}
                hint={absoluteTime(at)}
              />
              {took != null && (
                <TimeRow
                  label={t("entryPanel.took")}
                  value={humanDuration(took)}
                  hint={t("entryPanel.tookHint")}
                  testId="entry-panel-took"
                />
              )}
            </div>
          </section>
        )}
        {message.text ? (
          <section className="px-3 py-2.5">
            <h3 className={SECTION_TITLE}>{t("entryPanel.message")}</h3>
            <div className="mt-1.5 text-[0.8125rem] leading-snug" data-testid="entry-panel-text">
              <Markdown text={message.text} />
            </div>
          </section>
        ) : (
          <p className="px-3 py-6 text-center text-xs leading-relaxed text-faint">
            {t("entryPanel.noText")}
          </p>
        )}

        {message.thinking.length > 0 && (
          <section className="border-t px-3 py-2.5">
            <h3 className={SECTION_TITLE}>{t("entryPanel.thinking")}</h3>
            <p
              className="mt-1.5 text-[0.8125rem] leading-snug break-words whitespace-pre-wrap text-dim"
              data-testid="entry-panel-thinking"
            >
              {message.thinking.join("\n\n")}
            </p>
          </section>
        )}

        {message.toolCalls.length > 0 && (
          <section className="border-t px-3 py-2.5">
            <h3 className={SECTION_TITLE}>{t("entryPanel.toolCalls")}</h3>
            <ul className="mt-1.5 space-y-1">
              {message.toolCalls.map((call) => (
                <li
                  key={call.id}
                  className="flex items-baseline justify-between gap-3"
                  data-testid="entry-panel-tool"
                >
                  <span className="readout truncate text-xs">{call.name}</span>
                  {call.running ? (
                    <span className="legend">{t("entryPanel.running")}</span>
                  ) : (
                    // Issued at the end of the call that asked for it, which is
                    // the only interval there is: a tool *result* carries no
                    // stamps of its own.
                    call.endedAtMs != null &&
                    at > 0 &&
                    call.endedAtMs > at && (
                      <span className="legend whitespace-nowrap">
                        {humanDuration(call.endedAtMs - at)}
                      </span>
                    )
                  )}
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-2 border-t px-3 py-2">
        <button
          className="key key-flat !px-2 !py-1 text-xs"
          onClick={() => onOpenTranscript(message.id)}
          data-testid="entry-panel-open"
        >
          <MessageSquareText size={13} aria-hidden />
          {t("entryPanel.readInTranscript")}
        </button>
      </div>
    </SidePanel>
  );
}
