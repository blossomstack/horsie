
import { GlobalSessionStatusEvent } from './globalSessionStatusEvent';
import { GlobalSessionTitleEvent } from './globalSessionTitleEvent';
/**
 * One frame on the global `/api/events` stream (live session-list updates).
 */
export type GlobalSessionEvent =
  | { type: "StatusChanged"; value: GlobalSessionStatusEvent }
  | { type: "TitleChanged"; value: GlobalSessionTitleEvent };