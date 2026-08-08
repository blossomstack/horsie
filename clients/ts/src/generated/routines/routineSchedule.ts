
import { DailySchedule } from './dailySchedule';
import { EverySchedule } from './everySchedule';
import { ManualSchedule } from './manualSchedule';
import { MonthlySchedule } from './monthlySchedule';
import { OnceSchedule } from './onceSchedule';
import { WeeklySchedule } from './weeklySchedule';
import { YearlySchedule } from './yearlySchedule';
/**
 * When a routine fires by itself. A union rather than a kind + optional
 * fields, so "every, with no interval" cannot be expressed.
 */
export type RoutineSchedule =
  | { type: "Manual"; value: ManualSchedule }
  | { type: "Every"; value: EverySchedule }
  | { type: "Once"; value: OnceSchedule }
  | { type: "Daily"; value: DailySchedule }
  | { type: "Weekly"; value: WeeklySchedule }
  | { type: "Monthly"; value: MonthlySchedule }
  | { type: "Yearly"; value: YearlySchedule };