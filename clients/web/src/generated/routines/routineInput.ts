
import { EnvironmentSpec } from '../environments';
import { RoutineSchedule } from './routineSchedule';
/**
 * Create or fully replace a routine. `description` defaults to "", `schedule`
 * to Manual, and `enabled` to true.
 */
export interface RoutineInput {
  name: string;
  description?: string;
  agent: string;
  environment: EnvironmentSpec;
  prompt: string;
  schedule?: RoutineSchedule;
  enabled?: boolean;
}