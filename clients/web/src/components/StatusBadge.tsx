import { SessionStatusKind } from "../api/types";
import type { WorkflowStatus } from "../api/types";
import { cn } from "../lib/cn";
import {
  runStatusMeta,
  statusMeta,
  TONE_TEXT,
  type StatusTone,
} from "../lib/status";

/** The bare lamp, given a tone rather than a status: a session's status and a
 * run's are different vocabularies over the same panel, and the lamp is the
 * part they share. */
function Lamp({
  tone,
  busy,
  className,
}: {
  tone: StatusTone;
  busy: boolean;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "lamp",
        TONE_TEXT[tone],
        busy && "lamp-live",
        tone === "off" && "lamp-off",
        className,
      )}
      aria-hidden
    />
  );
}

/** A panel lamp. Never used alone to carry meaning — every lamp on this
 * console sits beside the word it stands for. */
export function StatusDot({
  status,
  className,
}: {
  status: SessionStatusKind | null | undefined;
  className?: string;
}) {
  const meta = statusMeta(status);
  return <Lamp tone={meta.tone} busy={meta.busy} className={className} />;
}

/** Lamp plus engraved legend, as it reads on the panel face. */
export function StatusBadge({
  status,
}: {
  status: SessionStatusKind | null | undefined;
}) {
  const meta = statusMeta(status);
  return (
    <span
      data-testid="status-badge"
      data-status={status ?? "Unknown"}
      className={cn("inline-flex items-center gap-2", TONE_TEXT[meta.tone])}
      title={meta.hint}
    >
      <StatusDot status={status} />
      <span className="legend !text-[0.625rem] text-current">{meta.label}</span>
    </span>
  );
}

/** The same panel face, reading a *run's* lifecycle instead of a session's.
 *
 * A run's outcome is durable, so this has no unknown state to fall back to: a
 * finished run reads Finished whether or not anything is still resident. */
export function RunStatusBadge({ status }: { status: WorkflowStatus }) {
  const meta = runStatusMeta(status);
  return (
    <span
      data-testid="run-status"
      data-status={status.type}
      className={cn("inline-flex items-center gap-2", TONE_TEXT[meta.tone])}
      title={meta.hint}
    >
      <Lamp tone={meta.tone} busy={meta.busy} />
      <span className="legend !text-[0.625rem] text-current">{meta.label}</span>
    </span>
  );
}
