import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta, TONE_TEXT } from "../lib/status";

/**
 * The session's status at the head of its title: a lamp AND the word.
 *
 * An earlier pass made this a bare glyph, which meant the word survived only
 * in the tooltip — and a tooltip is not something a sighted reader sees at a
 * glance. Carrying the word restores the standing rule that status is never
 * colour alone, and it costs about eight characters of title.
 */
/**
 * The session's status at the head of its title.
 *
 * Carries `data-testid` and `data-status` because this is now the one status
 * readout on the session header — the e2e suite's `expectStatus` reads them.
 */
export function StatusPill({ status }: { status: SessionStatusKind }) {
  const meta = statusMeta(status);
  return (
    <span
      data-testid="status-badge"
      data-status={status}
      className={cn("flex shrink-0 items-center gap-1.5", TONE_TEXT[meta.tone])}
      title={meta.hint}
    >
      <StatusDot status={status} />
      <span className="text-[0.75rem] font-medium text-current">{meta.label}</span>
    </span>
  );
}

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
