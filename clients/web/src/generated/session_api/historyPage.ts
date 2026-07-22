
import { Message } from '../agent';
import { TaskItem } from '../session';
import { UsageView } from '../session';
/**
 * One page of a session&#x27;s conversation history, served from the agent&#x27;s
 */
export interface HistoryPage {
  messages: Message[];
  hasMore: boolean;
  tasks?: TaskItem[];
  usage?: UsageView;
}