
import { TaskItem } from './taskItem';
/**
 * The agent's `task_list` state, whole. Both the current value and the
 * notification that it changed — a client folds it, last one wins.
 */
export interface TaskListLifecycle {
  tasks: TaskItem[];
}