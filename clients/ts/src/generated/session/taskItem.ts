
import { TaskStatus } from './taskStatus';
/**
 * One entry in the agent&#x27;s `task_list` tool state.
 */
export interface TaskItem {
  id: number;
  content: string;
  status: TaskStatus;
}