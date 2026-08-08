
/**
 * Triggered once at `at_ms` (unix epoch millis) and never re-armed. An
 * instant already in the past never fires; move it forward to re-arm.
 */
export interface OnceSchedule {
  atMs: number;
}