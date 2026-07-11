
import { Message } from '../agent';
/**
 * A complete transcript message (user, assistant, or tool result), replayed
 */
export interface MessageEvent {
  message: Message;
}