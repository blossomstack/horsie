
import { AgentLogEntry } from '../agent';
import { MessageDelta } from './messageDelta';
/**
 * One frame on `GET /sessions/:id/messages`.
 */
export type MessageFrame =
  | { type: "Entry"; value: AgentLogEntry }
  | { type: "Delta"; value: MessageDelta };