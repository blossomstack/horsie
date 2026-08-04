import type { SessionSummary } from "../api/types";

/** The frontend-only section for sessions without a group annotation. Never
 * sent to the API; a real group with this name is filtered out of the union. */
export const UNGROUPED = "ungrouped";

/** The session's group, from its `group` annotation. */
export function sessionGroup(s: SessionSummary): string | undefined {
  return s.annotations.find((a) => a.key === "group")?.value;
}

/** Every group the sidebar renders: registered groups plus names seen only in
 * annotations, deduped; `ungrouped` is a reserved word, never a real group. */
export function unionGroups(
  registered: string[],
  sessions: SessionSummary[],
): string[] {
  const known = new Set(registered);
  known.delete(UNGROUPED);
  const annotationOnly = new Set<string>();
  for (const s of sessions) {
    const g = sessionGroup(s);
    if (g && g !== UNGROUPED && !known.has(g)) annotationOnly.add(g);
  }
  return [...[...known].sort(), ...[...annotationOnly].sort()];
}

/** Bucket sessions by group; every listed group gets an entry, even empty. */
export function partitionSessions(
  sessions: SessionSummary[],
  groups: string[],
): Map<string, SessionSummary[]> {
  const parts = new Map<string, SessionSummary[]>();
  for (const g of [...groups, UNGROUPED]) parts.set(g, []);
  for (const s of sessions) {
    const g = sessionGroup(s);
    parts.get(g && parts.has(g) ? g : UNGROUPED)?.push(s);
  }
  return parts;
}

/** Merge the persisted order with the live group list: drop stale entries,
 * append new groups sorted, keep `ungrouped` exactly once. */
export function reconcileOrder(saved: string[], groups: string[]): string[] {
  const live = new Set([...groups, UNGROUPED]);
  const order = saved.filter((g, i) => live.has(g) && saved.indexOf(g) === i);
  const fresh = groups.filter((g) => !order.includes(g)).sort();
  order.push(...fresh);
  if (!order.includes(UNGROUPED)) order.push(UNGROUPED);
  return order;
}

/** Move `entry` to immediately before `target` (append if target is absent). */
export function moveBefore(
  order: string[],
  entry: string,
  target: string,
): string[] {
  const rest = order.filter((g) => g !== entry);
  const at = rest.indexOf(target);
  if (at === -1) return [...rest, entry];
  return [...rest.slice(0, at), entry, ...rest.slice(at)];
}
