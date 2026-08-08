
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
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}