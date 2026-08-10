import { memo } from "react";
import ReactMarkdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";

/**
 * Above this many characters, skip highlighting entirely.
 *
 * highlight.js is super-linear in the length of a single unbroken line: 3000
 * chars measured 448ms and 5000 measured 1230ms. A reply long enough to hit
 * this is one nobody is reading as syntax anyway.
 */
const HIGHLIGHT_MAX_CHARS = 40_000;

/**
 * Themed markdown renderer with GFM + syntax highlighting. Loaded lazily via
 * `./Prose` so highlight.js stays out of the initial bundle.
 *
 * **Highlighting is deliberately conditional**, because unconditional
 * highlighting hard-locked the browser tab: 100% CPU, no recovery, surviving
 * the turn being stopped server-side, needing `kill -9` on the renderer.
 *
 * Three things combined to do it. The plugin ran with `detect: true` —
 * highlight.js *auto-detection*, which runs every registered grammar over
 * every code block. This component is memoised on `text`, and a streamed reply
 * changes `text` on every token, so the whole pipeline re-ran over the whole
 * message hundreds of times. And the cost is super-linear in line length, so
 * 60 growing prefixes of one 3000-char line cost 9.3s of pure CPU — while real
 * streaming issues hundreds of updates, not 60.
 *
 * So: no `detect` (an unlabelled fence renders as plain text rather than
 * costing every grammar), nothing highlighted mid-stream, and nothing
 * highlighted above [`HIGHLIGHT_MAX_CHARS`]. The finished message highlights
 * once, which is the only render anyone actually reads.
 */
const Markdown = memo(function Markdown({
  text,
  highlight = true,
}: {
  text: string;
  /** False while the text is still arriving. */
  highlight?: boolean;
}) {
  const highlighting = highlight && text.length <= HIGHLIGHT_MAX_CHARS;
  return (
    <div className="prose">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={
          highlighting ? [[rehypeHighlight, { ignoreMissing: true }]] : []
        }
        components={{
          a: (props) => (
            <a {...props} target="_blank" rel="noreferrer noopener" />
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});

export default Markdown;
