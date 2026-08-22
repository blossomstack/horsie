import { Link } from "react-router-dom";
import type { RenderedSubSession } from "../hooks/useSessionStream";

/**
 * What the new session was given. A `fresh` sub session carries none of this
 * transcript — the agent wrote it a brief instead — so calling it "branched"
 * without qualification would suggest a history it does not have.
 */
function subSessionLabel(seed: string) {
  switch (seed) {
    case "summary":
      return "branched from here, with a summary";
    case "fresh":
      return "handed off from here";
    default:
      return "branched from here";
  }
}

/**
 * Where a session branched off, in the transcript of the one it left.
 *
 * A marker rather than a divider: a compaction boundary separates two working
 * sets and the thread genuinely breaks across it, but this session carried
 * straight on. What happened here is that another one started.
 *
 * The model never sees this — `prompt_messages` drops every lifecycle body —
 * which is why branching does not disturb the source's prompt cache.
 */
export function SubSessionMarker({
  value,
  sessionId,
}: {
  value: RenderedSubSession;
  sessionId: string;
}) {
  return (
    <div
      className="flex items-center gap-3 text-faint"
      data-testid="subSession-marker"
      data-subSession-id={value.id}
    >
      <span className="h-px flex-1 bg-[var(--rule)]" />
      <Link
        to={`/sessions/${sessionId}/agents/${value.id}`}
        className="legend whitespace-nowrap hover:text-legend"
      >
        {subSessionLabel(value.seed)}
        {" →"}
      </Link>
      <span className="h-px flex-1 bg-[var(--rule)]" />
    </div>
  );
}
