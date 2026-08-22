import {
  CheckCircle2,
  CircleAlert,
  CircleDashed,
  CircleDot,
  Loader,
  MessageCircleQuestion,
  type LucideIcon,
} from "lucide-react";
import { SessionStatusKind } from "../api/types";
import { cn } from "../lib/cn";
import { statusMeta, TONE_TEXT } from "../lib/status";

/**
 * A DISTINCT SHAPE per status, not one dot in six colours.
 *
 * The standing rule here is that status is never carried by colour alone, and
 * a coloured dot with the word beside it satisfies it. Moving the status to
 * the head of the session title means the word is no longer beside it — it is
 * in the tooltip and the accessible name, neither of which a sighted user
 * reading at a glance will see. So the glyph itself has to carry the meaning:
 * six statuses, six shapes, legible with the colour removed.
 */
const ICON: Record<SessionStatusKind, LucideIcon> = {
  [SessionStatusKind.Provisioning]: CircleDashed,
  [SessionStatusKind.Idle]: CircleDot,
  [SessionStatusKind.Running]: Loader,
  [SessionStatusKind.AwaitingInput]: MessageCircleQuestion,
  [SessionStatusKind.Finished]: CheckCircle2,
  [SessionStatusKind.Failed]: CircleAlert,
  [SessionStatusKind.Unrecoverable]: CircleAlert,
};

/**
 * The session's status at the head of its title.
 *
 * Carries `data-testid` and `data-status` because this is now the one status
 * readout on the session header — the e2e suite's `expectStatus` reads them.
 */
export function StatusIcon({ status }: { status: SessionStatusKind }) {
  const meta = statusMeta(status);
  const Icon = ICON[status];
  return (
    <span
      data-testid="status-badge"
      data-status={status}
      className={cn("flex shrink-0 items-center", TONE_TEXT[meta.tone])}
      title={`${meta.label} — ${meta.hint}`}
      aria-label={`Status: ${meta.label}`}
      role="img"
    >
      <Icon size={15} className={cn(meta.busy && "animate-spin [animation-duration:2s]")} />
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
