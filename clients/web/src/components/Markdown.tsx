import { memo } from "react";
import ReactMarkdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";

/**
 * Themed markdown renderer with GFM + syntax highlighting. Loaded lazily via
 * `./Prose` so highlight.js stays out of the initial bundle.
 */
const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="prose">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { detect: true, ignoreMissing: true }]]}
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
