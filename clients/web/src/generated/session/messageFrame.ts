
import { AgentLogEntry } from '../agent';
import { MessageDelta } from './messageDelta';
import { MessageWindow } from './messageWindow';
/**
 * One frame on `GET /sessions/:id/messages`.
 *
 * Two arms because there are two kinds of thing to carry, and they differ in
 * exactly one respect that matters: an `Entry` is durable and replayable, a
 * `Delta` is neither. A tagged union rather than "whichever field is present"
 * so a client's match is exhaustive and a third arm cannot be added silently.
 */
export type MessageFrame =
  | { type: "Window"; value: MessageWindow }
  | { type: "Entry"; value: AgentLogEntry }
  | { type: "Delta"; value: MessageDelta };