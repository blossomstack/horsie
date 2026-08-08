
/**
 * Triggered every `interval_secs` seconds. The next run is scheduled from
 * when the previous one fired, not from a fixed origin, so a server that was
 * down resumes with one run rather than a backlog. Minimum 60 seconds.
 */
export interface EverySchedule {
  intervalSecs: number;
}