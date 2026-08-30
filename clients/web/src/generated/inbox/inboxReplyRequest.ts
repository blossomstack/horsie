
/**
 * A reply typed into the inbox.
 *
 * One shape for both kinds: on an ask it becomes the answer to the parked
 * call, on a notice it becomes an ordinary message to that agent. The kind is
 * on the message, so the caller does not restate it and cannot get it wrong.
 */
export interface InboxReplyRequest {
  text: string;
}