
import { InboxMessageBody } from './inboxMessageBody';
import { InboxState } from './inboxState';
/**
 * One message, with everything needed to render and act on it without a
 * second request.
 */
export interface InboxMessageView {
  id: string;
  body: InboxMessageBody;
  state: InboxState;
  /**
   * Where it came from, and where "open the session" goes. `agent_id` is
   * `"main"` or an agent uuid — the same vocabulary every agent-scoped route
   * speaks, so it is usable as an address without translation.
   */
  sessionId: string;
  agentId: string;
  /**
   * A one-line summary for the list row. Set by the agent on a notice;
   * derived from the question on an ask.
   */
  title: string;
  createdAt: number;
  /**
   * When it was first opened. Absent means unread, which is what the badge
   * counts.
   */
  readAt?: number;
}