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
