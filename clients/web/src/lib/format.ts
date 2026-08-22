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
  const say = (v: string) => (future ? `in ${v}` : `${v} ago`);
  const s = Math.round(Math.abs(diff) / 1000);
  if (s < 45) return future ? "in a moment" : "just now";
  const m = Math.round(s / 60);
  if (m < 60) return say(`${m}m`);
  const h = Math.round(m / 60);
  if (h < 24) return say(`${h}h`);
  const d = Math.round(h / 24);
  if (d < 7) return say(`${d}d`);
  return new Date(epochMillis).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

/** Absolute local timestamp for tooltips. */
export function absoluteTime(epochMillis: number): string {
  return new Date(epochMillis).toLocaleString();
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
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m`;
  return `${Math.floor(ms / 3_600_000)}h ${Math.round((ms % 3_600_000) / 60_000)}m`;
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
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
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
  return name?.trim() || "New session";
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
