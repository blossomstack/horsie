
import { Weekday } from './weekday';
/**
 * On the listed weekdays at `hour:minute` in `timezone`. At least one day;
 * duplicates are rejected at save.
 */
export interface WeeklySchedule {
  timezone: string;
  hour: number;
  minute: number;
  weekdays: Weekday[];
}