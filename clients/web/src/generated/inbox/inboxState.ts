
/**
 * What has become of a message.
 *
 * `Open` is the only state that can still hold an agent, and only for an ask.
 * The three others are all terminal and differ in what the agent was told,
 * which is the thing a person re-reading their inbox actually wants to know.
 */
export enum InboxState {
  /**
   * Nothing has been done with it.
   */
  Open = "Open",
  /**
   * You answered the question, or replied to the notice. Answering anywhere
   * lands here — the session page and the inbox send the same command.
   */
  Answered = "Answered",
  /**
   * You declined the question. The agent was told nobody would answer and
   * resumed without one.
   */
  Declined = "Declined",
  /**
   * Never answered. An ask abandoned by a later message in its session, or
   * a notice archived without a reply.
   */
  Closed = "Closed",
}