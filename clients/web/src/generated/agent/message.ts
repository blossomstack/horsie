
import { ContentPart } from './contentPart';
import { Role } from './role';
/**
 * A single message in the conversation
 */
export interface Message {
  id: string;
  role: Role;
  parts: ContentPart[];
  /**
   * Unix-epoch ms when the server finalized this message — turn start for a
   * user message, stream completion for an assistant message, tool
   * completion for a tool result.
   */
  createdAtMs: number;
  /**
   * Unix-epoch ms when the provider call that produced this message was
   * issued. Assistant messages only; absent elsewhere.
   */
  startedAtMs?: number;
}