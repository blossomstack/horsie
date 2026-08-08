
/**
 * A user message the server has accepted but not yet answered. It rides on
 * the detail endpoint and on `InboxChanged` so a client can render it as
 * still-unanswered rather than as part of the transcript.
 */
export interface QueuedMessage {
  id: string;
  text: string;
  atMs: number;
}