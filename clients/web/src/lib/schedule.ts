import type { RoutineSchedule } from "../api/types";
import { i18n } from "../i18n";
import { localeTag } from "./format";

/** The shortest interval the server accepts (`MIN_INTERVAL_SECS`). */
export const MIN_INTERVAL_SECS = 60;

/** A month's short name in the language the interface is being read in.
 * `Intl` already knows every one of them, so the catalogue does not have to
 * carry twelve names per language. */
function monthName(month: number): string {
  return new Date(Date.UTC(2000, month - 1, 1)).toLocaleDateString(localeTag(), {
    month: "short",
    timeZone: "UTC",
  });
}

/** A weekday as the wire names it, in the reader's language. */
function weekdayName(day: string): string {
  const index = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].indexOf(day);
  if (index < 0) return day;
  // 2024-01-01 was a Monday, so the offset lands on the right day.
  return new Date(Date.UTC(2024, 0, 1 + index)).toLocaleDateString(localeTag(), {
    weekday: "short",
    timeZone: "UTC",
  });
}

/** "9" → "09", for clock and day rendering. */
function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/** "3" → "3rd"; the ordinal a calendar reader expects, in their language.
 * `Intl.PluralRules` picks the rule (English has four, Chinese has one), and
 * the catalogue supplies the wording for it. */
const ORDINAL_KEYS = {
  one: "schedule.ordinalOne",
  two: "schedule.ordinalTwo",
  few: "schedule.ordinalFew",
  other: "schedule.ordinalOther",
} as const;

function ordinal(n: number): string {
  const rule = new Intl.PluralRules(localeTag(), { type: "ordinal" }).select(n);
  const key = ORDINAL_KEYS[rule as keyof typeof ORDINAL_KEYS] ?? ORDINAL_KEYS.other;
  return i18n.t(key, { n });
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
  const at = (h: number, m: number) => `${pad2(h)}:${pad2(m)}`;
  switch (schedule.type) {
    case "Manual":
      return i18n.t("schedule.manually");
    case "Every":
      return i18n.t("schedule.every", {
        interval: formatInterval(schedule.value.intervalSecs),
      });
    case "Once":
      return i18n.t("schedule.once", {
        when: new Date(schedule.value.atMs).toLocaleString(localeTag(), {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        }),
      });
    case "Daily":
      return i18n.t("schedule.daily", {
        time: at(schedule.value.hour, schedule.value.minute),
        timezone: schedule.value.timezone,
      });
    case "Weekly":
      return i18n.t("schedule.weekly", {
        days: schedule.value.weekdays.map(weekdayName).join(", "),
        time: at(schedule.value.hour, schedule.value.minute),
        timezone: schedule.value.timezone,
      });
    case "Monthly":
      return i18n.t("schedule.monthly", {
        day: ordinal(schedule.value.dayOfMonth),
        time: at(schedule.value.hour, schedule.value.minute),
        timezone: schedule.value.timezone,
      });
    case "Yearly":
      return i18n.t("schedule.yearly", {
        month: monthName(schedule.value.month),
        day: ordinal(schedule.value.dayOfMonth),
        time: at(schedule.value.hour, schedule.value.minute),
        timezone: schedule.value.timezone,
      });
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
