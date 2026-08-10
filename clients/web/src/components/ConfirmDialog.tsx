import { useEffect, useRef, useSyncExternalStore } from "react";
import {
  answerConfirm,
  confirmSnapshot,
  subscribeConfirm,
} from "../lib/confirm";

/**
 * The one confirm the app raises, mounted once in `App`.
 *
 * Above the popover layer and above the failure notices, because it is modal:
 * nothing behind it can be acted on until it is answered. Escape and the
 * backdrop both cancel — a confirm you cannot back out of is worse than none —
 * and Cancel takes focus, so a held Return does not delete anything.
 */
export function ConfirmDialog() {
  const request = useSyncExternalStore(
    subscribeConfirm,
    confirmSnapshot,
    confirmSnapshot,
  );
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!request) return;
    cancelRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") answerConfirm(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [request]);

  if (!request) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 px-4"
      onClick={() => answerConfirm(false)}
      data-testid="confirm-backdrop"
    >
      <div
        className="panel w-full max-w-md p-4"
        role="alertdialog"
        aria-modal="true"
        aria-label="Confirm"
        onClick={(e) => e.stopPropagation()}
        data-testid="confirm-dialog"
      >
        <p className="text-sm leading-relaxed text-legend">{request.message}</p>
        <div className="mt-4 flex items-center justify-end gap-2">
          <button
            ref={cancelRef}
            type="button"
            className="key key-blank"
            onClick={() => answerConfirm(false)}
            data-testid="confirm-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            className="key key-stop"
            onClick={() => answerConfirm(true)}
            data-testid="confirm-accept"
          >
            {request.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
