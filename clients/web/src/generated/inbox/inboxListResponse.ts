
import { InboxMessageView } from './inboxMessageView';
/**
 * The inbox, as `GET /api/inbox` answers it.
 */
export interface InboxListResponse {
  /**
   * Newest first.
   */
  messages: InboxMessageView[];
  /**
   * Unread across the whole inbox, not just this page — a badge that only
   * counted the page would under-report the moment the list was paginated.
   */
  unread: number;
  /**
   * Asks still `Open`, across the whole inbox. Distinct from `unread` and
   * worth its own number: an unread notice costs nothing, an open ask is an
   * agent that has stopped.
   */
  openAsks: number;
}