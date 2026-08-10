/**
 * The app's own "are you sure?", in one place.
 *
 * Every destructive action but one used the browser's `window.confirm` while
 * group delete used a well-designed in-app confirm, so the same decision looked
 * like two different products depending on which row you clicked. The native
 * dialog is also unstyled, unthemed, cannot say more than one line, and hard
 * blocks the page — including browser automation, which is why the delete tests
 * all carry a `page.on("dialog")` handler.
 *
 * A module-level store rather than context, for the same reason
 * `mutationErrors` is one: a call site does `if (!(await askConfirm(…)))
 * return;`, which is a one-token change from the `confirm(…)` it replaces, and
 * needs no provider threaded to it. The dialog itself is mounted once, beside
 * `<MutationErrors/>` in `App`.
 */

export type ConfirmRequest = {
  id: number;
  message: string;
  /** Word on the confirming button — "Delete" reads better than "OK". */
  confirmLabel: string;
  resolve: (ok: boolean) => void;
};

let nextId = 1;
let current: ConfirmRequest | null = null;
const listeners = new Set<() => void>();

function emit() {
  for (const l of listeners) l();
}

export function subscribeConfirm(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function confirmSnapshot(): ConfirmRequest | null {
  return current;
}

/**
 * Ask, and resolve with what was pressed.
 *
 * One at a time: a second ask while one is open answers `false` rather than
 * queueing, because the only way to raise two is a stray double-fire and
 * silently confirming the second one later is the worst possible outcome.
 */
export function askConfirm(
  message: string,
  confirmLabel = "Delete",
): Promise<boolean> {
  if (current) return Promise.resolve(false);
  return new Promise<boolean>((resolve) => {
    current = { id: nextId++, message, confirmLabel, resolve };
    emit();
  });
}

/** Answer the open request. No-op when nothing is open. */
export function answerConfirm(ok: boolean) {
  const open = current;
  if (!open) return;
  current = null;
  emit();
  open.resolve(ok);
}

/** Test seam: the store outlives a component, so a suite has to reset it. */
export function resetConfirm() {
  current = null;
  nextId = 1;
  emit();
}
