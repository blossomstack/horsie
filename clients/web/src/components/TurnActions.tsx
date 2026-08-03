import { Check, Clipboard, Type } from "lucide-react";
import { useEffect, useRef, useState, type RefObject } from "react";
import { copyText, renderedTextOf } from "../lib/clipboard";
import { formatTime } from "../lib/time";

/**
 * The per-turn control row: when it happened, and two ways to take it with
 * you.
 *
 * Revealed on hover *and* on `focus-within`, and shown unconditionally on a
 * device with no hover at all (see `.turn-actions` in index.css). A control
 * that exists only under a pointer is a control keyboard and touch users do
 * not have — on a phone this would not have hidden the timestamp, it would
 * have deleted it.
 *
 * The channel labels ("You" / "Agent") and the always-visible timestamp that
 * used to occupy a 4.75rem gutter are gone: a user turn is a bordered bubble
 * and an agent turn is bare prose, so the roles were already legible without
 * being named on every entry.
 */
export function TurnActions({
  atMs,
  markdown,
  renderedRef,
}: {
  /** Absent for an optimistic echo or a queued message — neither has a server
   * stamp, and the local clock would misreport when the turn happened. */
  atMs?: number;
  /** The turn's markdown source. Absent on a user turn, whose text is already
   * plain, so that turn shows a single copy button. */
  markdown?: string;
  /** The rendered prose node, read for the plain-text copy. */
  renderedRef?: RefObject<HTMLDivElement | null>;
}) {
  const [copied, setCopied] = useState<"md" | "txt" | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  const flash = (which: "md" | "txt") => {
    setCopied(which);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(null), 1400);
  };

  const copyMarkdown = async () => {
    if (markdown === undefined) return;
    if (await copyText(markdown)) flash("md");
  };

  const copyPlain = async () => {
    const text = renderedRef
      ? renderedTextOf(renderedRef.current)
      : (markdown ?? "");
    if (await copyText(text)) flash("txt");
  };


  // A user turn's text is already plain, so a second button offering the same
  // string would be a choice without a difference.
  const plainOnly = markdown === undefined;

  return (
    /* Bottom-left, in the 1.75rem gap below the turn: it reads as belonging
       to the message it follows, sits where the eye already is at the end of
       a reply, and costs no reserved height. */
    <div
      className="turn-actions pointer-events-none absolute -bottom-6 left-0 z-10 flex items-center gap-1 opacity-0 transition-opacity focus-within:pointer-events-auto focus-within:opacity-100 group-hover:pointer-events-auto group-hover:opacity-100"
      data-testid="turn-actions"
    >
      {!plainOnly && (
        <button
          type="button"
          className="key-icon !h-6 !w-6"
          onClick={copyMarkdown}
          title="Copy as markdown"
          aria-label="Copy as markdown"
          data-testid="turn-copy-markdown"
        >
          {copied === "md" ? (
            <Check size={13} className="text-lamp-ok" aria-hidden />
          ) : (
            <Clipboard size={13} aria-hidden />
          )}
        </button>
      )}

      <button
        type="button"
        className="key-icon !h-6 !w-6"
        onClick={copyPlain}
        title={plainOnly ? "Copy" : "Copy as plain text"}
        aria-label={plainOnly ? "Copy" : "Copy as plain text"}
        data-testid="turn-copy-plain"
      >
        {copied === "txt" ? (
          <Check size={13} className="text-lamp-ok" aria-hidden />
        ) : plainOnly ? (
          <Clipboard size={13} aria-hidden />
        ) : (
          <Type size={13} aria-hidden />
        )}
      </button>

      {atMs !== undefined && (
        <span
          className="readout ml-0.5 text-[10px] tabular-nums"
          data-testid="turn-time"
          title={new Date(atMs).toLocaleString()}
        >
          {formatTime(atMs)}
        </span>
      )}

      {/* The copy result is a colour change on an icon, which says nothing to
          a screen reader; this is the word that goes with the lamp. */}
      <span className="sr-only" role="status">
        {copied === "md"
          ? "Markdown copied"
          : copied === "txt"
            ? "Text copied"
            : ""}
      </span>
    </div>
  );
}
