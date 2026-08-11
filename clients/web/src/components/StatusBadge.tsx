import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta, TONE_TEXT } from "../lib/status";

/** A panel lamp. Never used alone to carry meaning — every lamp on this
 * console sits beside the word it stands for. */
export function StatusDot({
  status,
  className,
}: {
  status: SessionStatusKind;
  className?: string;
}) {
  const meta = statusMeta(status);
  return (
    <span
      className={cn(
        "lamp",
        TONE_TEXT[meta.tone],
        meta.busy && "lamp-live",
        className,
      )}
      aria-hidden
    />
  );
}

/** Lamp plus engraved legend, as it reads on the panel face. */
export function StatusBadge({ status }: { status: SessionStatusKind }) {
  const meta = statusMeta(status);
  return (
    <span
      data-testid="status-badge"
      data-status={status}
      className={cn("inline-flex items-center gap-2", TONE_TEXT[meta.tone])}
      title={meta.hint}
    >
      <StatusDot status={status} />
      <span className="legend !text-[0.625rem] text-current">{meta.label}</span>
    </span>
  );
}
