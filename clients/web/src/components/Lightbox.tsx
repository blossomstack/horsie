import { Download, X } from "lucide-react";
import { useEffect, useRef, type KeyboardEvent } from "react";
import { useTranslation } from "react-i18next";

/** Everything a Tab can land on inside the dialog, in document order. */
const FOCUSABLE = "a[href], button:not(:disabled), [tabindex]:not([tabindex='-1'])";

/**
 * One image, full size, above everything else.
 *
 * Modelled on `ConfirmDialog`: same backdrop, same Escape, same "the first
 * control takes focus on open". It goes further in one way — the dialog traps
 * Tab. A confirm has two buttons and a page behind it that is about to be
 * acted on either way; this covers the transcript entirely, so a Tab that
 * escaped it would walk an invisible session rail with no way back but the
 * mouse.
 */
export function Lightbox({
  src,
  name,
  onClose,
}: {
  src: string;
  /** The filename, which is both the caption and what a download is called. */
  name: string;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDivElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    closeRef.current?.focus();
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Tab is handled here rather than on the document, so nothing outside the
  // dialog has to know it is open.
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== "Tab") return;
    const items = dialogRef.current?.querySelectorAll<HTMLElement>(FOCUSABLE);
    if (!items || items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    if (e.shiftKey ? active === first : active === last) {
      e.preventDefault();
      (e.shiftKey ? last : first).focus();
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4"
      onClick={onClose}
      data-testid="lightbox-backdrop"
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label={name}
        className="flex max-h-full max-w-full flex-col items-center gap-3"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
        data-testid="lightbox"
      >
        <img
          src={src}
          alt={name}
          // Bounded on both axes: a tall screenshot with only a width cap
          // pushes the download and close controls off the bottom of the
          // screen, which is the one thing a modal must never do.
          className="min-h-0 max-h-full max-w-full rounded-[var(--radius-panel)] object-contain"
        />
        <div className="flex items-center gap-2">
          <a
            href={src}
            download={name}
            className="key key-blank"
            data-testid="lightbox-download"
          >
            <Download size={13} aria-hidden />
            {t("artifact.download")}
          </a>
          <button
            ref={closeRef}
            type="button"
            className="key key-blank"
            onClick={onClose}
            aria-label={t("artifact.close")}
            data-testid="lightbox-close"
          >
            <X size={13} aria-hidden />
            {t("artifact.close")}
          </button>
        </div>
      </div>
    </div>
  );
}
