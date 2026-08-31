import { SessionStatusKind } from "../api/types";
import { i18n } from "../i18n";

/** The catalogue section a status reads its words out of. */
type StatusKey =
  | "provisioning"
  | "idle"
  | "running"
  | "awaitingInput"
  | "finished"
  | "failed"
  | "unrecoverable";

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

export interface StatusMeta {
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

/** What the composer reads while the session document is still in flight.
 *
 * Not a status: the server always has one. This is the client not knowing yet,
 * and sending stays enabled because sending is queued behind whatever the
 * session turns out to be doing. */
export const UNLOADED: StatusMeta = {
  label: "",
  tone: "idle",
  busy: false,
  canSend: true,
  hint: "",
};

/** Everything about a status that is not words. The words are looked up at
 * call time instead of baked in here — a module-level constant is evaluated
 * once at import, so a translated label captured here would still be in the
 * language the tab was opened in after a switch. */
const META: Record<
  SessionStatusKind,
  Omit<StatusMeta, "label" | "hint"> & { key: StatusKey }
> = {
  [SessionStatusKind.Provisioning]: {
    key: "provisioning",
    tone: "live",
    busy: true,
    canSend: true,
  },
  [SessionStatusKind.Idle]: {
    key: "idle",
    tone: "idle",
    busy: false,
    canSend: true,
  },
  [SessionStatusKind.Running]: {
    key: "running",
    tone: "live",
    busy: true,
    canSend: true,
  },
  [SessionStatusKind.AwaitingInput]: {
    key: "awaitingInput",
    tone: "attention",
    busy: false,
    canSend: true,
  },
  [SessionStatusKind.Finished]: {
    key: "finished",
    tone: "ready",
    busy: false,
    canSend: true,
  },
  [SessionStatusKind.Failed]: {
    key: "failed",
    tone: "fault",
    busy: false,
    canSend: true,
  },
  [SessionStatusKind.Unrecoverable]: {
    key: "unrecoverable",
    tone: "fault",
    busy: false,
    canSend: false,
  },
};

/** How a status reads on the panel.
 *
 * Total, and takes no absent value: the registry keeps a durable copy of every
 * session's status, so there is no session the server has nothing to say about.
 * There used to be — every row rendered an em dash until someone opened it —
 * and that "unknown" was never a state, only an empty cache. */
export function statusMeta(status: SessionStatusKind): StatusMeta {
  const { key, ...rest } = META[status];
  return {
    ...rest,
    label: i18n.t(`status.${key}.label`),
    hint: i18n.t(`status.${key}.hint`),
  };
}

/** An agent's status, in the session vocabulary the panel lamps speak.
 *
 * A sub session is a session, so it moves through the same states a session does
 * and reads on the same lamps — but it is an *agent*, so the server sends it in
 * the agent vocabulary. This is the one translation, in one place.
 *
 * `completed` maps to idle rather than getting a lamp of its own: only a
 * subagent or a workflow step reaches it, and neither is ever shown here. An
 * unknown value reads as idle too, so a status added server-side renders as a
 * quiet row rather than a blank one. */
export function agentStatusMeta(status: string): SessionStatusKind {
  switch (status) {
    case "provisioning":
      return SessionStatusKind.Provisioning;
    case "running":
      return SessionStatusKind.Running;
    case "awaiting_input":
      return SessionStatusKind.AwaitingInput;
    case "failed":
      return SessionStatusKind.Failed;
    default:
      return SessionStatusKind.Idle;
  }
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
    runtime_acquiring: i18n.t("progression.startingRuntime"),
    runtime_failed: i18n.t("progression.runtimeFailed"),
    // This turn's setup.
    acquiring_runtime: i18n.t("progression.startingRuntime"),
    scanning_workspace: i18n.t("progression.scanningWorkspace"),
    connecting_tools: i18n.t("progression.connectingTools"),
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
/**
 * Whether nothing can change without someone asking for it.
 *
 * `Finished` and `Failed` are a run's two resting places; `Unrecoverable` is
 * every session's. None of them is terminal — a retry moves all three — but
 * none of them moves on its own, which is what every poll and every Interrupt
 * control needs to know.
 */
export function settled(status: SessionStatusKind): boolean {
  return (
    status === SessionStatusKind.Finished ||
    status === SessionStatusKind.Failed ||
    status === SessionStatusKind.Unrecoverable
  );
}

export function showsProgression(stage: string | undefined): boolean {
  return stage !== undefined && !SETTLED.has(stage);
}

export const TONE_TEXT: Record<StatusTone, string> = {
  live: "text-live-ink",
  ready: "text-lamp-ok",
  idle: "text-dim",
  attention: "text-attention",
  fault: "text-red-ink",
  off: "text-faint",
};
