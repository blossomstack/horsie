import type { RoutineSchedule } from "../api/types";

/** The shortest interval the server accepts (`MIN_INTERVAL_SECS`). */
export const MIN_INTERVAL_SECS = 60;

/** A schedule in words: "manually", "every 30m", "once on Apr 4, 09:00". */
export function describeSchedule(schedule: RoutineSchedule): string {
  switch (schedule.type) {
    case "Manual":
      return "manually";
    case "Every":
      return `every ${formatInterval(schedule.value.intervalSecs)}`;
    case "Once":
      return `once on ${new Date(schedule.value.atMs).toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      })}`;
  }
}

/** Seconds as the coarsest whole unit that fits: "90s", "30m", "6h", "2d". */
export function formatInterval(seconds: number): string {
  if (seconds % 86_400 === 0) return `${seconds / 86_400}d`;
  if (seconds % 3_600 === 0) return `${seconds / 3_600}h`;
  if (seconds % 60 === 0) return `${seconds / 60}m`;
  return `${seconds}s`;
}

/** An epoch-millis instant as the value a `datetime-local` input wants
 * (local time, no zone suffix). */
export function toLocalInputValue(atMs: number): string {
  const d = new Date(atMs - new Date(atMs).getTimezoneOffset() * 60_000);
  return d.toISOString().slice(0, 16);
}

/** A `datetime-local` value back to epoch millis; NaN when unparseable. */
export function fromLocalInputValue(value: string): number {
  return new Date(value).getTime();
}
