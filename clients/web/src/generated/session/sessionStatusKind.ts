
/**
 * User-visible lifecycle state of a session. Failure reasons ride separately
 * in `last_error` so the enum stays a plain discriminant.
 */
export enum SessionStatusKind {
  /**
   * The runtime is being built. A session is created in this state and
   * leaves it once its vendor confirms the runtime; anything sent
   * meanwhile is queued and runs as soon as it does.
   */
  Provisioning = "Provisioning",
  Idle = "Idle",
  Running = "Running",
  AwaitingInput = "AwaitingInput",
  /**
   * A workflow run completed with no error. Not terminal: a retry or a new
   * message moves it back to `Running`.
   */
  Finished = "Finished",
  Failed = "Failed",
  Unrecoverable = "Unrecoverable",
}