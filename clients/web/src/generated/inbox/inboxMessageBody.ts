
import { AskBody } from './askBody';
import { NoticeBody } from './noticeBody';
/**
 * What a message is, and what only that kind carries.
 *
 * A union rather than a kind field plus optionals, so "an ask with no
 * question" and "a notice with a tool call id" cannot be written down.
 */
export type InboxMessageBody =
  | { kind: "Notice"; value: NoticeBody }
  | { kind: "Ask"; value: AskBody };