
/**
 * On `month`/`day_of_month` every year in `timezone`. Invalid dates
 * (Feb 29 in a non-leap year) recur only when valid.
 */
export interface YearlySchedule {
  timezone: string;
  hour: number;
  minute: number;
  month: number;
  dayOfMonth: number;
}