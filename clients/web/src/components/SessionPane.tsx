import type { ReactNode } from "react";
import { cn } from "../lib/cn";

/**
 * The pane the three session views share: transcript, timeline, graph.
 *
 * One component because they are one thing — the body of a session, in three
 * renderings — and they had drifted into three. The timeline and the graph
 * each painted `bg-chassis` on their own root while the transcript inherited
 * the column's `bg-panel`, so switching view changed the colour of the page
 * under a header that had not moved. Nobody decided that; it is what happens
 * when three components each answer "what am I sitting on?" for themselves.
 *
 * The ground belongs to the pane, so a fourth view gets it right by using
 * this and a change to it happens once.
 */
export function SessionPane({
  children,
  className,
  scroll = false,
  ...rest
}: {
  children: ReactNode;
  className?: string;
  /** The transcript scrolls itself, so it owns the scroller and the ref on
   * it; the timeline and the graph scroll inside their own content. */
  scroll?: boolean;
} & React.HTMLAttributes<HTMLDivElement> & {
    ref?: React.Ref<HTMLDivElement>;
  }) {
  return (
    <div
      className={cn(
        "min-h-0 flex-1 bg-panel",
        scroll && "relative overflow-y-auto",
        className,
      )}
      {...rest}
    >
      {children}
    </div>
  );
}
