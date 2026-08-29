import { i18n } from "../i18n";
import { localeTag } from "./format";

/** Formatting for the server-stamped timestamps that ride on every message
 * (`createdAtMs` / `startedAtMs`) and on tool-result events (`atMs`). */

/** A span, as a transcript reads it: "3m 12s", "45s", "1h 4m".
 *
 * Sub-second spans return null rather than "0s" — a tool that answered
 * instantly is better rendered with no duration at all than with a zero. */
export function formatDuration(ms: number): string | null {
  if (!Number.isFinite(ms) || ms < 1000) return null;
  const total = Math.round(ms / 1000);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  if (hours > 0) return i18n.t("time.hoursMinutesShort", { hours, minutes });
  if (minutes > 0)
    return i18n.t("time.minutesSecondsShort", { minutes, seconds });
  return i18n.t("time.secondsShort", { value: seconds });
}

/** Clock time for a turn boundary — "14:32". Same-day assumption is
 * deliberate: the date belongs to the session, not to every bubble in it. */
export function formatTime(atMs: number): string {
  return new Date(atMs).toLocaleTimeString(localeTag(), {
    hour: "2-digit",
    minute: "2-digit",
  });
}
