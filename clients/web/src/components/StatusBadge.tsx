import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta, TONE_TEXT } from "../lib/status";

/**
 * The session's status at the head of its title.
 *
 * The lamp alone here, with the word in the tooltip and the accessible name.
 * This is the one place the "a lamp AND a word" rule is relaxed, and it is
 * relaxed deliberately: the header carries the title of the thing you are
 * looking at, and a second reading of a state the rail already spells out
 * beside every session cost more than it told anyone. Every OTHER lamp in
 * the build still has its word.
 */
/**
 * The session's status at the head of its title.
 *
 * Carries `data-testid` and `data-status` because this is now the one status
 * readout on the session header — the e2e suite's `expectStatus` reads them.
 */
export function StatusLamp({ status }: { status: SessionStatusKind }) {
  const meta = statusMeta(status);
  return (
    <span
      data-testid="status-badge"
      data-status={status}
      className={cn("flex shrink-0 items-center", TONE_TEXT[meta.tone])}
      title={`${meta.label} — ${meta.hint}`}
      aria-label={`Status: ${meta.label}`}
      role="img"
    >
      <StatusDot status={status} />
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
