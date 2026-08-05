/**
 * The editor sidebar's list arithmetic, kept out of the component.
 *
 * Selection is by step id rather than by index: a reorder must not change what
 * is on screen, and an index-based selection silently follows whichever step
 * slid into the slot. Ids are client-side only — they never reach the API,
 * which knows a step by its name.
 */

export type Selection = { kind: "definition" } | { kind: "step"; id: string };

export const DEFINITION: Selection = { kind: "definition" };

/** Whether `sel` points at this step. */
export function isSelected(sel: Selection, id: string): boolean {
  return sel.kind === "step" && sel.id === id;
}

/**
 * Move the item at `from` so it sits at `to`, shifting the rest.
 *
 * Out-of-range indices return the list untouched: a drop outside the list and
 * an arrow key at either end are both ordinary, not errors.
 */
export function moveItem<T>(list: T[], from: number, to: number): T[] {
  if (from === to) return list;
  if (from < 0 || from >= list.length) return list;
  if (to < 0 || to >= list.length) return list;
  const next = list.slice();
  const [item] = next.splice(from, 1);
  next.splice(to, 0, item);
  return next;
}

/**
 * What to select once the step at `index` is gone.
 *
 * Selection only moves when the removed step was the selected one; removing
 * some other step must not yank the panel away from what is being edited. The
 * step that slides into the freed slot takes over, or the one before it when
 * the last was removed, or the definition when nothing is left.
 */
export function afterRemoval(
  ids: string[],
  index: number,
  current: Selection,
): Selection {
  const removed = ids[index];
  if (removed === undefined) return current;
  if (!isSelected(current, removed)) return current;
  const rest = ids.filter((_, i) => i !== index);
  const next = rest[index] ?? rest[index - 1];
  return next === undefined ? DEFINITION : { kind: "step", id: next };
}
