
/**
 * Triggered once at `at_ms` (unix epoch millis) and never re-armed. An
 */
export interface OnceSchedule {
  atMs: number;
}