import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta, TONE_TEXT } from "../lib/status";

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
        "inline-block h-2 w-2 shrink-0 rounded-full bg-current",
        TONE_TEXT[meta.tone],
        meta.busy && "dot-pulse",
        className,
      )}
      aria-hidden
    />
  );
}

export function StatusBadge({ status }: { status: SessionStatusKind }) {
  const meta = statusMeta(status);
  return (
    <span
      data-testid="status-badge"
      data-status={status}
      className={cn(
        "inline-flex items-center gap-2 rounded-full border px-2.5 py-1 text-xs font-medium",
        TONE_TEXT[meta.tone],
      )}
      title={meta.hint}
    >
      <StatusDot status={status} />
      {meta.label}
    </span>
  );
}
