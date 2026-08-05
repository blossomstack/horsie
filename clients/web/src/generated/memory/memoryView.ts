
/**
 * One memory, body included. Addressed by the agent as `&#60;space&#62;/&#60;name&#62;`.
 */
export interface MemoryView {
  id: number;
  space: string;
  name: string;
  /**
   * One line, shown in the agent&#39;s prompt index.
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