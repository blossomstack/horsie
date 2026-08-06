import { SessionStatusKind } from "../api/types";

/** Panel lamp colours. `live` is a value in motion, `attention` is a control
 * waiting on the operator, `fault` is a stopped machine, `off` is an
 * unlit lamp — a channel the server has nothing to report for. */
export type StatusTone = "live" | "ready" | "attention" | "fault" | "off";

interface StatusMeta {
  label: string;
  tone: StatusTone;
  /** The agent is actively doing work — used to animate the status dot. */
  busy: boolean;
  /** Whether a user message can be sent in this state. Only a terminally
   * broken session says no: a message sent during a turn is queued, and a
   * failed turn is retried by sending again. */
  canSend: boolean;
  hint: string;
}

/** A session the server has no status for: nothing is loaded at boot, so a
 * session reports nothing until it is opened. Shown as an em dash rather than
 * a guess — and still sendable, since sending is what loads it. */
export const UNKNOWN_STATUS: StatusMeta = {
  label: "—",
  tone: "off",
  busy: false,
  canSend: true,
  hint: "Not loaded — send a message to open it.",
};

const META: Record<SessionStatusKind, StatusMeta> = {
  [SessionStatusKind.Provisioning]: {
    label: "Provisioning",
    tone: "live",
    busy: true,
    canSend: true,
    hint: "Building this session's runtime — anything you send runs as soon as it is up.",
  },
  [SessionStatusKind.Idle]: {
    label: "Idle",
    tone: "ready",
    busy: false,
    canSend: true,
    hint: "Ready for your next message.",
  },
  [SessionStatusKind.Running]: {
    label: "Running",
    tone: "live",
    busy: true,
    canSend: true,
    hint: "The agent is working — anything you send is answered next turn.",
  },
  [SessionStatusKind.AwaitingInput]: {
    label: "Awaiting input",
    tone: "attention",
    busy: false,
    canSend: true,
    hint: "The agent asked you a question.",
  },
  [SessionStatusKind.Failed]: {
    label: "Failed",
    tone: "fault",
    busy: false,
    canSend: true,
    hint: "The last turn failed — send a message to try again.",
  },
  [SessionStatusKind.Unrecoverable]: {
    label: "Unrecoverable",
    tone: "fault",
    busy: false,
    canSend: false,
    hint: "This session's runtime is gone for good. Start a new session.",
  },
};

export function statusMeta(
  status: SessionStatusKind | null | undefined,
): StatusMeta {
  if (!status) return UNKNOWN_STATUS;
  return META[status] ?? UNKNOWN_STATUS;
}

export const TONE_TEXT: Record<StatusTone, string> = {
  live: "text-amber-ink",
  ready: "text-lamp-ok",
  attention: "text-orange",
  fault: "text-red-ink",
  off: "text-faint",
};
