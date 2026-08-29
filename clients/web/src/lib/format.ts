import { i18n, resolveLocale } from "../i18n";

/** The BCP-47 tag `Intl` should format with — the language the interface is
 * being read in, not the browser's, so a date under a Chinese UI is a Chinese
 * date even in an English-locale browser. */
export function localeTag(): string {
  return resolveLocale(
    (i18n.language as "en" | "zh-Hans" | "zh-Hant") ?? "en",
  );
}

/**
 * Compact relative time, e.g. "just now", "3m ago", "in 2h", "Apr 4".
 *
 * Both directions, because this formats future timestamps too. It used to
 * subtract and never look at the sign, so a routine's `nextRunAtMs` produced a
 * negative diff that fell straight into the `< 45s` branch: every armed
 * routine, on every schedule, on both list and detail, read **"next just
 * now"**. The one number that says a routine is going to fire was
 * unconditionally wrong, in the most reassuring direction.
 */
export function relativeTime(epochMillis: number): string {
  const diff = Date.now() - epochMillis;
  const future = diff < 0;
  const say = (value: string) =>
    future ? i18n.t("time.in", { value }) : i18n.t("time.ago", { value });
  const s = Math.round(Math.abs(diff) / 1000);
  if (s < 45)
    return future ? i18n.t("time.inAMoment") : i18n.t("time.justNow");
  const m = Math.round(s / 60);
  if (m < 60) return say(i18n.t("time.minutesShort", { value: m }));
  const h = Math.round(m / 60);
  if (h < 24) return say(i18n.t("time.hoursShort", { value: h }));
  const d = Math.round(h / 24);
  if (d < 7) return say(i18n.t("time.daysShort", { value: d }));
  return new Date(epochMillis).toLocaleDateString(localeTag(), {
    month: "short",
    day: "numeric",
  });
}

/** Absolute local timestamp for tooltips. */
export function absoluteTime(epochMillis: number): string {
  return new Date(epochMillis).toLocaleString(localeTag());
}

/**
 * A stretch of time, in the shortest unit that stays honest: "740ms", "3.4s",
 * "12m", "2h 5m".
 *
 * One copy. The timeline's bars, the graph's nodes and the agent panel all
 * draw durations, and two of the three had grown their own identical private
 * version of this.
 */
export function humanDuration(ms: number): string {
  if (ms < 1000)
    return i18n.t("time.millisecondsShort", { value: Math.round(ms) });
  if (ms < 60_000)
    return i18n.t("time.secondsShort", { value: (ms / 1000).toFixed(1) });
  if (ms < 3_600_000)
    return i18n.t("time.minutesShort", { value: Math.round(ms / 60_000) });
  return i18n.t("time.hoursMinutesShort", {
    hours: Math.floor(ms / 3_600_000),
    minutes: Math.round((ms % 3_600_000) / 60_000),
  });
}

/** A moment, 24-hour, so a label is five characters wide however long the
 * session ran. */
export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/** Group-thousands integer formatting. */
export function compactNumber(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000)
    return i18n.t("format.thousands", { value: (n / 1000).toFixed(n < 10_000 ? 1 : 0) });
  return i18n.t("format.millions", { value: (n / 1_000_000).toFixed(1) });
}

/** Last path segment of a workdir, for compact display. */
export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx >= 0 ? trimmed.slice(idx + 1) || trimmed : trimmed;
}

/** Display title for a session: its name once titled, else a plain
 * placeholder (never the raw uuid — nobody wants to scan session ids). */
export function sessionTitle(name: string | undefined): string {
  return name?.trim() || i18n.t("session.untitled");
}

const TITLE_MAX_CHARS = 60;

/**
 * A short title derived from a user's first message — mirrors the server's
 * own derivation (session_actor.rs `derive_title`) so an unnamed session's
 * title appears instantly on send instead of waiting for the next refetch.
 */
export function deriveTitle(text: string): string | null {
  const firstLine = (text.split("\n")[0] ?? "").trim();
  if (!firstLine) return null;
  if (firstLine.length <= TITLE_MAX_CHARS) return firstLine;
  return `${firstLine.slice(0, TITLE_MAX_CHARS).trimEnd()}…`;
}
