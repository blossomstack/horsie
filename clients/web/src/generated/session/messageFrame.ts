
import { AgentLogEntry } from '../agent';
import { MessageDelta } from './messageDelta';
import { MessageWindow } from './messageWindow';
/**
 * One frame on `GET /sessions/:id/messages`.
 */
export type MessageFrame =
  | { type: "Window"; value: MessageWindow }
  | { type: "Entry"; value: AgentLogEntry }
  | { type: "Delta"; value: MessageDelta };