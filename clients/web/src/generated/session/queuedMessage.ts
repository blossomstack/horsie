
/**
 * A user message the server has accepted but not yet answered. It rides on
 */
export interface QueuedMessage {
  id: string;
  text: string;
  atMs: number;
}