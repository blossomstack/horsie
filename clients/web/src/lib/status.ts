import { SessionStatusKind } from "../api/types";
import type { WorkflowStatus } from "../api/types";

/** Panel lamp colours. `live` is a value in motion, `attention` is a control
 * waiting on the operator, `fault` is a stopped machine, `idle` is a subdued
 * ready session, and `off` is an unlit lamp — a channel the server has
 * nothing to report for. */
export type StatusTone =
  | "live"
  | "ready"
  | "idle"
  | "attention"
  | "fault"
  | "off";

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
    tone: "idle",
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

/** Friendly label for a progression stage.
 *
 * The keys are the server's own stage vocabulary, and there are two sources of
 * it: a `Preparing` entry carries the stage the context provider named
 * (`acquiring_runtime`, `scanning_workspace`, `connecting_tools`, `ready`),
 * while a `Runtime` entry is folded into `runtime_<status>`. Both sit in this
 * one map because both land in the same line on screen — and a key that is in
 * neither vocabulary is how the whole provisioning wait came to render as
 * "runtime acquiring…".
 *
 * Unknown stages still fall back to a de-slugged form, so a stage added
 * server-side reads sensibly before this map hears about it. */
export function progressionLabel(stage: string): string {
  const known: Record<string, string> = {
    // The session's sandbox.
    runtime_acquiring: "Starting runtime…",
    runtime_failed: "Runtime failed",
    // This turn's setup.
    acquiring_runtime: "Starting runtime…",
    scanning_workspace: "Scanning workspace…",
    connecting_tools: "Connecting tools…",
  };
  return known[stage] ?? `${stage.replace(/_/g, " ")}…`;
}

/** Stages that have settled: the wait is over and there is nothing to report.
 *
 * Both ends of it, which is the fix — a turn's setup finishing (`ready`) and
 * the sandbox coming up (`runtime_ready`) are the same non-news said by the two
 * different sources, and only the first was ever hidden. The second sat above
 * the composer as a lamp and the words "runtime ready…" until a turn began. */
const SETTLED = new Set(["ready", "runtime_ready"]);

/** Whether a stage is worth a line on screen. */
export function showsProgression(stage: string | undefined): boolean {
  return stage !== undefined && !SETTLED.has(stage);
}

/** How a run reads on the panel.
 *
 * A run's lifecycle is not a session's: a session is idle between turns and a
 * run is not, and a run is the only thing that can be finished. So it gets its
 * own vocabulary over the same lamps — one place, because the run page and the
 * workflow's list of past runs have to agree about what "Suspended" looks
 * like. */
const RUN_META: Record<WorkflowStatus["type"], RunStatusMeta> = {
  Pending: {
    label: "Pending",
    tone: "off",
    busy: false,
    hint: "Created; no step has started yet.",
  },
  Running: {
    label: "Running",
    tone: "live",
    busy: true,
    hint: "A step is working.",
  },
  Suspended: {
    label: "Suspended",
    tone: "attention",
    busy: false,
    hint: "A step was interrupted — nothing runs until you retry it.",
  },
  AwaitingInput: {
    label: "Awaiting input",
    tone: "attention",
    busy: false,
    hint: "A step is parked on a question.",
  },
  Finished: {
    label: "Finished",
    tone: "ready",
    busy: false,
    hint: "The run reached a terminal step and carried its output out.",
  },
  Failed: {
    label: "Failed",
    tone: "fault",
    busy: false,
    hint: "The run ended on an error no retry clears by itself.",
  },
};

export interface RunStatusMeta {
  label: string;
  tone: StatusTone;
  busy: boolean;
  hint: string;
}

/** A run always has one: unlike a session's, its lifecycle state is durable, so
 * there is no unknown to fall back to. */
export function runStatusMeta(status: WorkflowStatus): RunStatusMeta {
  return (
    RUN_META[status.type] ?? {
      label: status.type,
      tone: "off",
      busy: false,
      hint: "",
    }
  );
}

export const TONE_TEXT: Record<StatusTone, string> = {
  live: "text-amber-ink",
  ready: "text-lamp-ok",
  idle: "text-dim",
  attention: "text-orange",
  fault: "text-red-ink",
  off: "text-faint",
};
