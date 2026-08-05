
import { TaskItem } from './taskItem';
/**
 * The agent&#39;s `task_list` tool state, sent whole on every mutation
 */
export interface TaskListEvent {
  tasks: TaskItem[];
}