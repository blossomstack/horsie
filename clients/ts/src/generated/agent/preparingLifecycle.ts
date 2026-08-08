
/**
 * How far this turn's setup has got: `scanning_workspace`, `connecting_tools`,
 * `ready`. Narration, and free-form on purpose — nothing acts on it, and the
 * stages differ per vendor.
 */
export interface PreparingLifecycle {
  stage: string;
  detail?: string;
}