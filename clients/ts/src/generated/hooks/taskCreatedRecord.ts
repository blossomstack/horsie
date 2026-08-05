
import { TaskOutcome } from './taskOutcome';
export interface TaskCreatedRecord {
  taskId: string;
  systemMessage?: string;
  outcome: TaskOutcome;
}