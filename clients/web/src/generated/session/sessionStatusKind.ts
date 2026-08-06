
/**
 * User-visible lifecycle state of a session. Failure reasons ride separately
 */
export enum SessionStatusKind {
  Provisioning = "Provisioning",
  Idle = "Idle",
  Running = "Running",
  AwaitingInput = "AwaitingInput",
  Failed = "Failed",
  Unrecoverable = "Unrecoverable",
}