
import { AgentTreeEvent } from './agentTreeEvent';
import { ErrorEvent } from './errorEvent';
import { InboxChangedEvent } from './inboxChangedEvent';
import { ProgressionEvent } from './progressionEvent';
import { StatusChangedEvent } from './statusChangedEvent';
/**
 * A frame on the session stream (`/sessions/:id/events`). Session-scoped
 */
export type SessionEvent =
  | { type: "StatusChanged"; value: StatusChangedEvent }
  | { type: "InboxChanged"; value: InboxChangedEvent }
  | { type: "Error"; value: ErrorEvent }
  | { type: "Progressed"; value: ProgressionEvent }
  | { type: "AgentTreeChanged"; value: AgentTreeEvent };