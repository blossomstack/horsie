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
 * Blocks that carry the reply's own text, and therefore its own direction.
 *
 * react-markdown emits a bare `<p>`, which inherits the app's `ltr`, so an
 * Arabic or Hebrew reply rendered flush left: the browser's bidi algorithm got
 * the word order right and the paragraph still read as visibly wrong to anyone
 * who reads that way. `dir="auto"` puts each block in the direction of its own
 * first strong character, which is per block rather than per message — a reply
 * that quotes English inside Hebrew gets both right.
 */
const directionalComponents = {
  p: (props: object) => <p dir="auto" {...props} />,
  li: (props: object) => <li dir="auto" {...props} />,
  h1: (props: object) => <h1 dir="auto" {...props} />,
  h2: (props: object) => <h2 dir="auto" {...props} />,
  h3: (props: object) => <h3 dir="auto" {...props} />,
  h4: (props: object) => <h4 dir="auto" {...props} />,
  h5: (props: object) => <h5 dir="auto" {...props} />,
  h6: (props: object) => <h6 dir="auto" {...props} />,
  blockquote: (props: object) => <blockquote dir="auto" {...props} />,
  td: (props: object) => <td dir="auto" {...props} />,
  th: (props: object) => <th dir="auto" {...props} />,
};

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
  compact = false,
}: {
  text: string;
  /** False while the text is still arriving. */
  highlight?: boolean;
  /** Scaled down for a side panel — see `.prose-compact`. */
  compact?: boolean;
}) {
  const highlighting = highlight && text.length <= HIGHLIGHT_MAX_CHARS;
  return (
    <div className={compact ? "prose prose-compact" : "prose"}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={
          highlighting ? [[rehypeHighlight, { ignoreMissing: true }]] : []
        }
        components={{
          ...directionalComponents,
          a: (props) => (
            <a {...props} target="_blank" rel="noreferrer noopener" />
          ),
          // A wide table used to push the whole transcript column sideways, so
          // recovering one cell meant dragging every other message with it.
          // The scroll belongs to the table, not to the session.
          table: (props) => (
            <div className="prose-scroll">
              <table {...props} />
            </div>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});

export default Markdown;
