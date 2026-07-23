
/**
 * A resource-preparation progression — shown live while a turn spins up and
 */
export interface ProgressionEvent {
  stage: string;
  detail?: string;
  atMs: number;
}