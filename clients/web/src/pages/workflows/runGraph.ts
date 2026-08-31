import type { SessionStatusKind, StepRunView } from "../../api/types";
import { SessionStatusKind as Status } from "../../api/types";

/**
 * Reading a run's graph: the questions a run page asks of it, apart from how
 * it is drawn.
 *
 * Its own module because a run is no longer a page. The graph is now one of a
 * session's three views, and this is the part of the old run page that was
 * never layout — what the run is waiting on, where it stopped, whether a retry
 * would race something.
 */

/** Whether retrying would race a step that is already writing the workspace.
 *
 * The server can make a retry safe by cancelling the active step first, but
 * that is not what a Retry button should silently do. Keep it unavailable
 * until the run settles, and cover a stale session document with the attempt's
 * own live status. */
export function retryUnavailable(
  status: SessionStatusKind,
  retryPending: boolean,
  step?: StepRunView,
): boolean {
  return (
    retryPending ||
    status === Status.Running ||
    step?.status.type === "Running"
  );
}

/** A run's output as text: a string is its own answer, anything else is JSON.
 *
 * The same rule the server uses to hand one step's output to the next, so what
 * is read here is what a following step would have been given. */
export function formatOutput(output: unknown): string {
  return typeof output === "string" ? output : JSON.stringify(output, null, 2);
}
