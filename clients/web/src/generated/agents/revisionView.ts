
/**
 * One past version of a preset or a memory.
 */
export interface RevisionView {
  revision: number;
  /**
   * The entity's whole wire shape at this revision, as JSON.
   */
  payload: string;
  /**
   * This revision recorded a deletion; `payload` is what was deleted.
   */
  deleted: boolean;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
}