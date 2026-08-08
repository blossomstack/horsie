
/**
 * Acknowledges an action with nothing to report — stop, delete. Its own type
 * rather than a `SessionAck` carrying an empty id, which read on the wire as a
 * message the client should be able to match and never could.
 */
export interface Ack {
}