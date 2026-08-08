
/**
 * On `day_of_month` of every month in `timezone`. Months without that day
 * (the 31st, the 29th–31st in February) are skipped entirely.
 */
export interface MonthlySchedule {
  timezone: string;
  hour: number;
  minute: number;
  dayOfMonth: number;
}