import { X } from "lucide-react";
import { useSyncExternalStore } from "react";
import {
  dismissMutationError,
  snapshot,
  subscribe,
} from "../api/mutationErrors";

/**
 * Failed writes, anchored to the viewport.
 *
 * Viewport-anchored on purpose. The cloud-vendor page put its error banner at
 * the top of a long form and the SAVE button 900px below it, so the banner
 * rendered off-screen and SAVE appeared to do nothing at all. A notice you
 * have to scroll to find is a notice that was not delivered.
 *
 * Above the popover layer (`z-30`) because a failure has to be readable over
 * whatever was open when it happened.
 */
export function MutationErrors() {
  const errors = useSyncExternalStore(subscribe, snapshot, snapshot);
  if (errors.length === 0) return null;

  return (
    <div
      className="pointer-events-none fixed inset-x-0 bottom-0 z-40 flex flex-col items-center gap-2 px-4 pb-4"
      // Announced rather than merely drawn: the composer keeps focus after a
      // failed send, so nothing else would tell a screen reader.
      role="status"
      aria-live="polite"
      data-testid="mutation-errors"
    >
      {errors.map((e) => (
        <div
          key={e.id}
          className="panel pointer-events-auto flex w-full max-w-3xl items-start gap-3 border-red bg-red-quiet px-3 py-2.5"
          data-testid="mutation-error"
        >
          <div className="min-w-0 flex-1">
            {/* Never colour alone: the word says it failed even where the red
              does not read. */}
            <h2 className="legend text-red-ink">Failed</h2>
            <p className="mt-1 text-sm leading-relaxed break-words text-red-ink">
              {e.message}
            </p>
          </div>
          <button
            className="key key-flat shrink-0"
            onClick={() => dismissMutationError(e.id)}
            aria-label="Dismiss"
            data-testid="mutation-error-dismiss"
          >
            <X size={13} aria-hidden />
          </button>
        </div>
      ))}
    </div>
  );
}
