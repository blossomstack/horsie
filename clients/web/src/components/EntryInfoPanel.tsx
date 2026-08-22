import { ExternalLink } from "lucide-react";
import type { RenderedMessage } from "../hooks/useSessionStream";
import Markdown from "./Markdown";
import { SidePanel } from "./SidePanel";

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
  return (
    <SidePanel
      legend="Entry"
      readout={
        <span className="readout text-[0.6875rem]" data-testid="entry-panel-readout">
          {role.toLowerCase()}
        </span>
      }
      onClose={onClose}
      closeLabel="Hide the entry panel"
      testId="entry-panel"
      closeTestId="entry-panel-collapse"
    >
      <div className="min-h-0 flex-1 overflow-y-auto">
        {message.text ? (
          <section className="px-3 py-2.5">
            <h3 className="legend !text-faint">Message</h3>
            <div className="mt-1.5 text-[0.8125rem] leading-snug" data-testid="entry-panel-text">
              <Markdown text={message.text} />
            </div>
          </section>
        ) : (
          <p className="px-3 py-6 text-center text-xs leading-relaxed text-faint">
            This entry carries no text of its own — it is the work it set off.
          </p>
        )}

        {message.thinking.length > 0 && (
          <section className="border-t px-3 py-2.5">
            <h3 className="legend !text-faint">Thinking</h3>
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
            <h3 className="legend !text-faint">Tool calls</h3>
            <ul className="mt-1.5 space-y-1">
              {message.toolCalls.map((call) => (
                <li
                  key={call.id}
                  className="flex items-baseline justify-between gap-3"
                  data-testid="entry-panel-tool"
                >
                  <span className="readout truncate text-xs">{call.name}</span>
                  {call.running && <span className="legend">running</span>}
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
          <ExternalLink size={13} aria-hidden />
          Read in transcript
        </button>
      </div>
    </SidePanel>
  );
}
