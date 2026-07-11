/** Compact relative time, e.g. "just now", "3m ago", "2h ago", "Apr 4". */
export function relativeTime(epochMillis: number): string {
  const diff = Date.now() - epochMillis;
  const s = Math.round(diff / 1000);
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24);
  if (d < 7) return `${d}d ago`;
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
