
import { TaskItem } from './taskItem';
/**
 * The agent&#39;s `task_list` state, whole. Both the current value and the
 */
export interface TaskListLifecycle {
  tasks: TaskItem[];
}