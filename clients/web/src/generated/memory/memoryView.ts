
/**
 * One memory, body included. Addressed by the agent as `<space>/<name>`.
 */
export interface MemoryView {
  id: number;
  space: string;
  name: string;
  /**
   * One line, shown in the agent's prompt index.
   */
  description: string;
  /**
   * Markdown body, loaded on demand.
   */
  content: string;
  /**
   * Which version of this memory this is. Pass it back as
   * `expected_revision` on an update and the update is refused if anything
   * changed in between. Absent on a memory that predates versioning and has
   * not been written since.
   */
  revision?: number;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}