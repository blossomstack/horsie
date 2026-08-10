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
}: {
  text: string;
  /** Suppresses syntax highlighting until the text stops changing — see
   *  `Markdown` for why that mattered enough to thread a flag down here. */
  streaming?: boolean;
}) {
  return (
    <Suspense fallback={<div className="prose whitespace-pre-wrap">{text}</div>}>
      <Markdown text={text} highlight={!streaming} />
    </Suspense>
  );
}
