
/**
 * Acknowledges an accepted user message. The id is how a client matches its
 * optimistic bubble to the queued message the server now owes an answer for.
 */
export interface SessionAck {
  messageId: string;
}