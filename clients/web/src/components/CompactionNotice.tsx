import { FoldVertical } from "lucide-react";
import type { RenderedCompactionSkip } from "../hooks/useSessionStream";
import { compactNumber } from "../lib/format";

/** A `/compact` that was asked for and found nothing to fold.
 *
 * The same rule-with-a-label as a real boundary, in the faint ink that says
 * nothing moved — a reader scanning for where the thread breaks must not stop
 * here, because it does not break here.
 *
 * It exists because the alternative is silence. Typing a command and getting no
 * response at all reads as a broken feature, which is exactly how this was
 * reported. Not a button: there is nothing to expand, and the whole account
 * fits in a line and its tooltip. */
export function CompactionNotice({ value }: { value: RenderedCompactionSkip }) {
  // Without a declared context window there is no budget, so the only honest
  // thing to say is that nothing was folded.
  const explanation =
    value.retainTokens === null
      ? "This model declares no context window, so there is no budget to compact against."
      : `This session is about ${compactNumber(value.contextTokens)} tokens and a compaction keeps the most recent ${compactNumber(value.retainTokens)} verbatim — so there is nothing before that to fold. Compacting anyway would trade real messages for a summary to buy room that is not scarce.`;

  return (
    <div data-testid="compaction-notice" className="py-1">
      <div className="flex items-center gap-3">
        <span className="h-px flex-1 bg-[var(--rule)]" aria-hidden />
        <span
          className="flex shrink-0 items-center gap-1.5 rounded-full border border-[var(--rule)] px-2.5 py-1 text-[0.6875rem] uppercase tracking-wide text-faint"
          title={explanation}
        >
          <FoldVertical size={11} aria-hidden />
          <span>
            Nothing to compact
            {value.retainTokens !== null && (
              <>
                {" "}
                · {compactNumber(value.contextTokens)} of{" "}
                {compactNumber(value.retainTokens)} tokens kept
              </>
            )}
          </span>
        </span>
        <span className="h-px flex-1 bg-[var(--rule)]" aria-hidden />
      </div>
    </div>
  );
}
