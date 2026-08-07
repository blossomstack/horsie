
import { Weekday } from './weekday';
/**
 * On the listed weekdays at `hour:minute` in `timezone`. At least one day;
 */
export interface WeeklySchedule {
  timezone: string;
  hour: number;
  minute: number;
  weekdays: Weekday[];
}