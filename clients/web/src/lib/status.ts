import { SessionStatusKind } from "../api/types";

export type StatusTone = "accent" | "success" | "warning" | "error" | "muted";

interface StatusMeta {
  label: string;
  tone: StatusTone;
  /** The agent is actively doing work — used to animate the status dot. */
  busy: boolean;
  /** Whether a user message can be sent in this state. */
  canSend: boolean;
  hint: string;
}

const META: Record<SessionStatusKind, StatusMeta> = {
  [SessionStatusKind.Provisioning]: {
    label: "Provisioning",
    tone: "warning",
    busy: true,
    canSend: false,
    hint: "Spinning up the runtime sandbox…",
  },
  [SessionStatusKind.Idle]: {
    label: "Idle",
    tone: "success",
    busy: false,
    canSend: true,
    hint: "Ready for your next message.",
  },
  [SessionStatusKind.Running]: {
    label: "Running",
    tone: "accent",
    busy: true,
    canSend: false,
    hint: "The agent is working on your request.",
  },
  [SessionStatusKind.AwaitingInput]: {
    label: "Awaiting input",
    tone: "warning",
    busy: false,
    canSend: true,
    hint: "The agent asked you a question.",
  },
  [SessionStatusKind.Interrupted]: {
    label: "Interrupted",
    tone: "warning",
    busy: false,
    canSend: true,
    hint: "Recovered after a restart — send a message to resume.",
  },
  [SessionStatusKind.Stopped]: {
    label: "Stopped",
    tone: "muted",
    busy: false,
    canSend: true,
    hint: "Runtime preserved — send a message to reattach.",
  },
  [SessionStatusKind.RecoveryFailed]: {
    label: "Recovery failed",
    tone: "error",
    busy: false,
    canSend: true,
    hint: "Could not reattach the runtime — retry by sending a message.",
  },
  [SessionStatusKind.Failed]: {
    label: "Failed",
    tone: "error",
    busy: false,
    canSend: false,
    hint: "The session failed.",
  },
};

export function statusMeta(status: SessionStatusKind): StatusMeta {
  return META[status] ?? META[SessionStatusKind.Idle];
}

export const TONE_TEXT: Record<StatusTone, string> = {
  accent: "text-accent",
  success: "text-success",
  warning: "text-warning",
  error: "text-error",
  muted: "text-faint",
};
