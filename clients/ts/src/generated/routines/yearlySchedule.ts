
/**
 * On `month`/`day_of_month` every year in `timezone`. Invalid dates
 */
export interface YearlySchedule {
  timezone: string;
  hour: number;
  minute: number;
  month: number;
  dayOfMonth: number;
}