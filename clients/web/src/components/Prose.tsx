import { lazy, Suspense } from "react";

const Markdown = lazy(() => import("./Markdown"));

/**
 * Lazy markdown renderer. Until the markdown chunk loads it shows the raw text
 * (which for streaming output is usually indistinguishable), so there is no
 * visible flash.
 */
export function Prose({ text }: { text: string }) {
  return (
    <Suspense
      fallback={
        <div className="prose whitespace-pre-wrap">{text}</div>
      }
    >
      <Markdown text={text} />
    </Suspense>
  );
}
