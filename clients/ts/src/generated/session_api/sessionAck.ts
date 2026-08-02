
/**
 * Acknowledges an accepted user message. The id is how a client matches its
 */
export interface SessionAck {
  messageId: string;
}