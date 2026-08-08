
import { AgentLogEntry } from '../agent';
/**
 * One page of an agent's log — the `before`/`max` form of
 * `GET /sessions/:id/messages`.
 *
 * No `has_more` in either direction: fewer entries than `max` means there are
 * no more. The flag was a second way to say the same thing, and on a forward
 * page it could not distinguish "nothing newer" from "nothing newer yet".
 */
export interface MessagesPage {
  entries: AgentLogEntry[];
}