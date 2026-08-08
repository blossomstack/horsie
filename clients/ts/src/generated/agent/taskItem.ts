
import { TaskStatus } from './taskStatus';
/**
 * One entry in the agent's `task_list` tool state.
 */
export interface TaskItem {
  id: number;
  content: string;
  status: TaskStatus;
}