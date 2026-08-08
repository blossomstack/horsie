
/**
 * Current lifecycle state of a runtime
 */
export enum RuntimeState {
  Creating = "Creating",
  Running = "Running",
  Stopping = "Stopping",
  Stopped = "Stopped",
  Failed = "Failed",
}