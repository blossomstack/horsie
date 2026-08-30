
/**
 * Which messages to act on. Every mutation takes a set, because the list is
 * the place these are used and selecting several there is the ordinary case.
 */
export interface InboxMessageIds {
  ids: string[];
}