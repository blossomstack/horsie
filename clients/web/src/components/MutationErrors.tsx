import { X } from "lucide-react";
import { useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
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
  const { t } = useTranslation();
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
          className="notice notice-fault pointer-events-auto w-full max-w-3xl"
          data-testid="mutation-error"
        >
          <div className="notice-body">
            {/* Never colour alone: the word says it failed even where the red
              does not read. */}
            <h2 className="legend text-current">{t("mutationErrors.failed")}</h2>
            <p className="mt-1 break-words text-current">{e.message}</p>
          </div>
          <button
            className="key-icon shrink-0"
            onClick={() => dismissMutationError(e.id)}
            aria-label={t("common.dismiss")}
            data-testid="mutation-error-dismiss"
          >
            <X size={13} aria-hidden />
          </button>
        </div>
      ))}
    </div>
  );
}
