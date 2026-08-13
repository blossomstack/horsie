import { Link } from "react-router-dom";
import type { RenderedFork } from "../hooks/useSessionStream";

/**
 * Where a conversation branched off, in the transcript of the one it left.
 *
 * A marker rather than a divider: a compaction boundary separates two working
 * sets and the thread genuinely breaks across it, but this conversation carried
 * straight on. What happened here is that another one started.
 *
 * The model never sees this — `prompt_messages` drops every lifecycle body —
 * which is why forking does not disturb the source's prompt cache.
 */
export function ForkMarker({
  value,
  sessionId,
}: {
  value: RenderedFork;
  sessionId: string;
}) {
  return (
    <div
      className="flex items-center gap-3 text-faint"
      data-testid="fork-marker"
      data-fork-id={value.id}
    >
      <span className="h-px flex-1 bg-[var(--rule)]" />
      <Link
        to={`/sessions/${sessionId}/agents/${value.id}`}
        className="legend whitespace-nowrap hover:text-legend"
      >
        {value.mode === "summary"
          ? "forked from here, with a summary"
          : "forked from here"}
        {" →"}
      </Link>
      <span className="h-px flex-1 bg-[var(--rule)]" />
    </div>
  );
}
