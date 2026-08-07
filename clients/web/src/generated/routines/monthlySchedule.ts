
/**
 * On `day_of_month` of every month in `timezone`. Months without that day
 */
export interface MonthlySchedule {
  timezone: string;
  hour: number;
  minute: number;
  dayOfMonth: number;
}