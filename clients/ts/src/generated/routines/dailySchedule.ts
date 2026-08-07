
/**
 * Every day at `hour:minute` in `timezone`.
 */
export interface DailySchedule {
  timezone: string;
  hour: number;
  minute: number;
}