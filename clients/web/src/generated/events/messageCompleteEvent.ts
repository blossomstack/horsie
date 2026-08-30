
import { Message } from '../agent';
export interface MessageCompleteEvent {
  messageId: string;
  message: Message;
}