
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
  Failed = "Failed",
  Unrecoverable = "Unrecoverable",
}