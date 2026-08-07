import type { RoutineSchedule } from "../api/types";

/** The shortest interval the server accepts (`MIN_INTERVAL_SECS`). */
export const MIN_INTERVAL_SECS = 60;

const MONTHS = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun",
  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
] as const;

/** "9" → "09", for clock and day rendering. */
function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/** "3" → "3rd"; the ordinal suffix a calendar reader expects. */
function ordinal(n: number): string {
  const last = n % 10;
  const suffix = last === 1 ? "st" : last === 2 ? "nd" : last === 3 ? "rd" : "th";
  return `${n}${suffix}`;
}

/** The browser's IANA timezone; the form's default. */
export function browserTimezone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
}

const FALLBACK_ZONES = [
  "UTC",
  "America/New_York",
  "America/Los_Angeles",
  "Europe/London",
  "Europe/Berlin",
  "Asia/Shanghai",
  "Asia/Tokyo",
  "Australia/Sydney",
];

/** IANA zones for the timezone picker, alphabetized, with a curated fallback
 * for engines without `Intl.supportedValuesOf`. */
export function timezoneOptions(): string[] {
  const zones = Intl.supportedValuesOf?.("timeZone") ?? [];
  if (zones.length === 0) return FALLBACK_ZONES;
  const all = [...zones];
  if (!all.includes(browserTimezone())) all.push(browserTimezone());
  return all.sort();
}

/** A schedule in words: "manually", "every 30m", "once on Apr 4, 09:00",
 * "every Mon, Wed, Fri at 09:00 (Asia/Shanghai)". */
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
    case "Daily":
      return `daily at ${pad2(schedule.value.hour)}:${pad2(schedule.value.minute)} (${schedule.value.timezone})`;
    case "Weekly":
      return `every ${schedule.value.weekdays.join(", ")} at ${pad2(schedule.value.hour)}:${pad2(schedule.value.minute)} (${schedule.value.timezone})`;
    case "Monthly":
      return `monthly on the ${ordinal(schedule.value.dayOfMonth)} at ${pad2(schedule.value.hour)}:${pad2(schedule.value.minute)} (${schedule.value.timezone})`;
    case "Yearly":
      return `yearly on ${MONTHS[schedule.value.month - 1]} ${ordinal(schedule.value.dayOfMonth)} at ${pad2(schedule.value.hour)}:${pad2(schedule.value.minute)} (${schedule.value.timezone})`;
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
