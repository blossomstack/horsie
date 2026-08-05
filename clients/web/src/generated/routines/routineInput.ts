
import { RoutineSchedule } from './routineSchedule';
/**
 * Create or fully replace a routine. `description` defaults to &#34;&#34;, `schedule`
 */
export interface RoutineInput {
  name: string;
  description?: string;
  agent: string;
  prompt: string;
  schedule?: RoutineSchedule;
  enabled?: boolean;
}