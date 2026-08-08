
/**
 * A resource-preparation progression — shown live while a turn spins up and
 * kept for audit. `stage` is a stable key (`acquiring_runtime`,
 * `scanning_workspace`, `connecting_tools`, `ready`); `detail` is optional
 * human text; `at_ms` is the unix-epoch millisecond it occurred.
 */
export interface ProgressionEvent {
  stage: string;
  detail?: string;
  atMs: number;
}