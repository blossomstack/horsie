/**
 * Every failed write, in one place.
 *
 * Testing found ~8 mutations calling `.mutate()` with no `onError`, so a
 * perfectly good `{code,message}` vanished and the only symptom was a row that
 * did not disappear. `409 agent_in_use` *names the routine that is blocking
 * the delete* and the user never saw it.
 *
 * Handled once, in the `MutationCache`, rather than at each call site: a
 * call-site list is a denylist that silently re-opens the moment someone adds
 * the 35th mutation. A site that renders its own inline error opts out with
 * `meta: { inlineError: true }`.
 *
 * A module-level store rather than context, because the emitter is the query
 * client — created outside the React tree — and threading a provider back to
 * it would invert the dependency for no gain.
 */

export type MutationError = {
  id: number;
  message: string;
};

let nextId = 1;
let current: MutationError[] = [];
const listeners = new Set<() => void>();

/** How many to keep. A burst of failures is one problem, not nine. */
const MAX = 3;

function emit() {
  for (const l of listeners) l();
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function snapshot(): MutationError[] {
  return current;
}

export function pushMutationError(message: string) {
  const trimmed = message.trim();
  if (!trimmed) return;
  // A retry that fails the same way should not stack up N identical notices.
  const duplicate = current.find((e) => e.message === trimmed);
  if (duplicate) {
    current = [...current.filter((e) => e !== duplicate), duplicate];
    emit();
    return;
  }
  current = [...current, { id: nextId++, message: trimmed }].slice(-MAX);
  emit();
}

export function dismissMutationError(id: number) {
  current = current.filter((e) => e.id !== id);
  emit();
}

/** Test seam: the store outlives a component, so a suite has to reset it. */
export function resetMutationErrors() {
  current = [];
  nextId = 1;
  emit();
}
