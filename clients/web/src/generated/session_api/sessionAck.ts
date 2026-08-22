
/**
 * Acknowledges an accepted user message. The id is how a client matches its
 * optimistic bubble to the queued message the server now owes an answer for.
 *
 * `sub_session` names the one a `/fork` or `/summary-n-fork` just created, for
 * the client to open. Absent for every ordinary message, which is what makes
 * the field additive — and a field here rather than a second endpoint, so
 * every client that can send a message can branch without learning a new call.
 */
export interface SessionAck {
  messageId: string;
  subSession?: string;
}