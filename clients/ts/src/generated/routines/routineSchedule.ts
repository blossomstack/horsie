
import { EverySchedule } from './everySchedule';
import { ManualSchedule } from './manualSchedule';
import { OnceSchedule } from './onceSchedule';
/**
 * When a routine fires by itself. A union rather than a kind + optional
 */
export type RoutineSchedule =
  | { type: "Manual"; value: ManualSchedule }
  | { type: "Every"; value: EverySchedule }
  | { type: "Once"; value: OnceSchedule };