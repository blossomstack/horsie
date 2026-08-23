import { lazy, Suspense } from "react";

// Start fetching the markdown chunk the moment this module is evaluated, not
// when the first <Prose> mounts. Without it, reopening a session rendered the
// whole transcript as *raw* markdown — asterisks, pipe tables, fence markers —
// until the chunk landed. Deferring the warm to requestIdleCallback was not
// early enough; the fallback still won the race on a cold load.
const chunk = import("./Markdown");
const Markdown = lazy(() => chunk);

/**
 * Lazy markdown renderer. Until the markdown chunk loads it shows the raw text
 * (which for streaming output is usually indistinguishable), so there is no
 * visible flash.
 */
export function Prose({
  text,
  streaming = false,
  compact = false,
}: {
  text: string;
  /** Suppresses syntax highlighting until the text stops changing — see
   *  `Markdown` for why that mattered enough to thread a flag down here. */
  streaming?: boolean;
  /** Read in a side panel rather than the transcript column: same rendering,
   *  scaled to the panel's own voice. */
  compact?: boolean;
}) {
  const cls = compact ? "prose prose-compact" : "prose";
  return (
    <Suspense fallback={<div className={`${cls} whitespace-pre-wrap`}>{text}</div>}>
      <Markdown text={text} highlight={!streaming} compact={compact} />
    </Suspense>
  );
}
