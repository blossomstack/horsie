
/**
 * Omitted fields are left unchanged; supplied ones are replaced wholesale.
 */
export interface MemoryUpdateInput {
  description?: string;
  content?: string;
  /**
   * Refuse this write unless the memory is still at this revision.
   *
   * Absent writes unconditionally. Supply it whenever you read, decide, and
   * write back — a curating agent and the session that owns the memory can
   * otherwise overwrite each other with no trace that either did.
   */
  expectedRevision?: number;
}