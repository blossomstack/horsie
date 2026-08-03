
import { RoutineSchedule } from './routineSchedule';
/**
 * Create or fully replace a routine. `description` defaults to &quot;&quot;, `schedule`
 */
export interface RoutineInput {
  name: string;
  description?: string;
  agent: string;
  prompt: string;
  schedule?: RoutineSchedule;
  enabled?: boolean;
}