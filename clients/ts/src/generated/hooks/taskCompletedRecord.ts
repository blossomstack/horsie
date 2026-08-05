
import { TaskOutcome } from './taskOutcome';
export interface TaskCompletedRecord {
  taskId: string;
  systemMessage?: string;
  outcome: TaskOutcome;
}