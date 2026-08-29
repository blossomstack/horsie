
import { Role } from '../agent';
/**
 * Assistant message lifecycle
 */
export interface MessageStartEvent {
  messageId: string;
  role: Role;
}