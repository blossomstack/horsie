//! Next-occurrence math for the calendar schedule arms (daily, weekly,
//! monthly, yearly). Pure functions over the wire `RoutineSchedule`; the
//! scheduler reaches them through `service::next_run_at`, never directly.
//!
//! Each arm computes the first instant strictly after `now` at which its
//! wall-clock rule fires, in the schedule's own timezone. Candidates that
//! fall on days the rule cannot express — the 31st of a 30-day month, 29
//! February in a non-leap year — are skipped rather than clamped, matching
//! how a calendar user reads "on the 31st".
//!
//! DST uses jiff's default `Compatible` disambiguation: a wall-clock time
//! that lands in a spring-forward gap fires at the shifted time that same
//! day; a fall-back repeat fires once. The calendar never skips a day for
//! DST.

use horsie_models::routines::{
    DailySchedule, MonthlySchedule, RoutineSchedule, Weekday, WeeklySchedule, YearlySchedule,
};
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

/// The first instant (unix epoch millis) strictly after `now_ms` at which
/// `schedule` fires, or `None` when it never will: a manual/every/once
/// schedule (the caller handles those), or a timezone that cannot be
/// resolved (corrupt storage — save-time validation blocks this).
pub fn next_occurrence(schedule: &RoutineSchedule, now_ms: u64) -> Option<u64> {
    let now = Timestamp::from_millisecond(now_ms as i64).ok()?;
    match schedule {
        RoutineSchedule::Daily(d) => daily(d, now),
        RoutineSchedule::Weekly(w) => weekly(w, now),
        RoutineSchedule::Monthly(m) => monthly(m, now),
        RoutineSchedule::Yearly(y) => yearly(y, now),
        RoutineSchedule::Manual(_) | RoutineSchedule::Every(_) | RoutineSchedule::Once(_) => None,
    }
}

fn daily(d: &DailySchedule, now: Timestamp) -> Option<u64> {
    let tz = TimeZone::get(&d.timezone).ok()?;
    let today = now.to_zoned(tz.clone()).date();
    [today, today.tomorrow().ok()?]
        .into_iter()
        .find_map(|date| occurrence_after(date, &tz, d.hour, d.minute, now))
}

fn weekly(w: &WeeklySchedule, now: Timestamp) -> Option<u64> {
    let tz = TimeZone::get(&w.timezone).ok()?;
    let today = now.to_zoned(tz.clone()).date();
    let days: Vec<i8> = w.weekdays.iter().map(weekday_offset).collect();
    let mut date = today;
    // Seven distinct weekdays plus the wrap-around: a bounded scan.
    for _ in 0..8 {
        if days.contains(&date.weekday().to_monday_one_offset())
            && let Some(ms) = occurrence_after(date, &tz, w.hour, w.minute, now)
        {
            return Some(ms);
        }
        date = date.tomorrow().ok()?;
    }
    None
}

fn monthly(m: &MonthlySchedule, now: Timestamp) -> Option<u64> {
    let tz = TimeZone::get(&m.timezone).ok()?;
    let start = now.to_zoned(tz.clone()).date();
    // Walk the first of each month; `Date::new` rejects months that lack
    // `day_of_month`, which is exactly the skip rule. The scan is bounded:
    // the longest gap between valid candidates is ~3 months (e.g. 31 Jan →
    // 31 Mar), plus the current month being tested.
    let mut first = Date::new(start.year(), start.month(), 1).ok()?;
    for _ in 0..14 {
        if let Some(date) = Date::new(first.year(), first.month(), m.day_of_month as i8).ok()
            && let Some(ms) = occurrence_after(date, &tz, m.hour, m.minute, now)
        {
            return Some(ms);
        }
        first = first.checked_add(1.month()).ok()?;
    }
    None
}

fn yearly(y: &YearlySchedule, now: Timestamp) -> Option<u64> {
    let tz = TimeZone::get(&y.timezone).ok()?;
    let start = now.to_zoned(tz.clone()).date();
    let mut year = start.year();
    // A leap-day rule can skip three years between valid candidates (2026,
    // 2027, 2028…), plus the year being tested: eight keeps the scan bounded.
    for _ in 0..8 {
        if let Some(date) = Date::new(year, y.month as i8, y.day_of_month as i8).ok()
            && let Some(ms) = occurrence_after(date, &tz, y.hour, y.minute, now)
        {
            return Some(ms);
        }
        year += 1;
    }
    None
}

/// The firing at `hour:minute` on `date` strictly after `now`, if any.
fn occurrence_after(
    date: Date,
    tz: &TimeZone,
    hour: u32,
    minute: u32,
    now: Timestamp,
) -> Option<u64> {
    let dt = DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        hour as i8,
        minute as i8,
        0,
        0,
    )
    .ok()?;
    let ts = dt.to_zoned(tz.clone()).ok()?.timestamp();
    (ts > now).then(|| ts.as_millisecond() as u64)
}

/// Our `Weekday` as jiff's Monday-based offset (Mon=1 … Sun=7).
fn weekday_offset(d: &Weekday) -> i8 {
    match d {
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
        Weekday::Sun => 7,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::routines::{
        DailySchedule, EverySchedule, ManualSchedule, MonthlySchedule, OnceSchedule,
        WeeklySchedule, YearlySchedule,
    };

    /// `civil` wall-clock string ("2026-08-07T09:00") in `zone`, as epoch millis.
    fn at(zone: &str, civil: &str) -> u64 {
        let dt: jiff::civil::DateTime = civil.parse().unwrap();
        dt.to_zoned(jiff::tz::TimeZone::get(zone).unwrap())
            .unwrap()
            .timestamp()
            .as_millisecond() as u64
    }

    fn daily(zone: &str, hour: u32, minute: u32) -> RoutineSchedule {
        RoutineSchedule::Daily(DailySchedule {
            timezone: zone.into(),
            hour,
            minute,
        })
    }

    fn weekly(zone: &str, hour: u32, minute: u32, weekdays: &[Weekday]) -> RoutineSchedule {
        RoutineSchedule::Weekly(WeeklySchedule {
            timezone: zone.into(),
            hour,
            minute,
            weekdays: weekdays.to_vec(),
        })
    }

    fn monthly(zone: &str, hour: u32, minute: u32, day_of_month: u32) -> RoutineSchedule {
        RoutineSchedule::Monthly(MonthlySchedule {
            timezone: zone.into(),
            hour,
            minute,
            day_of_month,
        })
    }

    fn yearly(
        zone: &str,
        hour: u32,
        minute: u32,
        month: u32,
        day_of_month: u32,
    ) -> RoutineSchedule {
        RoutineSchedule::Yearly(YearlySchedule {
            timezone: zone.into(),
            hour,
            minute,
            month,
            day_of_month,
        })
    }

    #[test]
    fn daily_fires_today_if_ahead_else_tomorrow() {
        // 2026-08-07 is a Friday. EDT is UTC-4 in August.
        assert_eq!(
            next_occurrence(
                &daily("America/New_York", 9, 0),
                at("America/New_York", "2026-08-07T06:00")
            ),
            Some(at("America/New_York", "2026-08-07T09:00"))
        );
        assert_eq!(
            next_occurrence(
                &daily("America/New_York", 9, 0),
                at("America/New_York", "2026-08-07T10:00")
            ),
            Some(at("America/New_York", "2026-08-08T09:00"))
        );
    }

    #[test]
    fn weekly_fires_on_the_next_matching_weekday_and_wraps() {
        let mwf = weekly(
            "Asia/Shanghai",
            9,
            0,
            &[Weekday::Mon, Weekday::Wed, Weekday::Fri],
        );
        // 2026-08-07 is Friday; 04:00 CST is still before 09:00.
        assert_eq!(
            next_occurrence(&mwf, at("Asia/Shanghai", "2026-08-07T04:00")),
            Some(at("Asia/Shanghai", "2026-08-07T09:00"))
        );
        // Friday 10:00 has passed → next is Monday.
        assert_eq!(
            next_occurrence(&mwf, at("Asia/Shanghai", "2026-08-07T10:00")),
            Some(at("Asia/Shanghai", "2026-08-10T09:00"))
        );
    }

    #[test]
    fn monthly_skips_months_without_the_day() {
        // April has 30 days: the 31st skips to May 31.
        assert_eq!(
            next_occurrence(&monthly("UTC", 0, 0, 31), at("UTC", "2026-04-15T00:00")),
            Some(at("UTC", "2026-05-31T00:00"))
        );
        // 2026 is not a leap year: Feb 29 does not exist.
        assert_eq!(
            next_occurrence(&monthly("UTC", 0, 0, 29), at("UTC", "2026-02-10T00:00")),
            Some(at("UTC", "2026-03-29T00:00"))
        );
        assert_eq!(
            next_occurrence(&monthly("UTC", 9, 0, 15), at("UTC", "2026-08-07T12:00")),
            Some(at("UTC", "2026-08-15T09:00"))
        );
    }

    #[test]
    fn yearly_skips_invalid_dates_and_recurves_only_when_valid() {
        // Feb 29 next exists in 2028.
        assert_eq!(
            next_occurrence(&yearly("UTC", 0, 0, 2, 29), at("UTC", "2025-03-01T00:00")),
            Some(at("UTC", "2028-02-29T00:00"))
        );
        // Same-year candidate still ahead.
        assert_eq!(
            next_occurrence(&yearly("UTC", 0, 0, 2, 29), at("UTC", "2028-02-28T12:00")),
            Some(at("UTC", "2028-02-29T00:00"))
        );
        assert_eq!(
            next_occurrence(&yearly("UTC", 9, 0, 12, 25), at("UTC", "2026-08-07T00:00")),
            Some(at("UTC", "2026-12-25T09:00"))
        );
    }

    #[test]
    fn dst_gap_shifts_and_fold_fires_once() {
        // 2026-03-08 02:30 EST does not exist (EDT starts 02:00→03:00);
        // Compatible disambiguation fires at 03:30 EDT that day.
        assert_eq!(
            next_occurrence(
                &daily("America/New_York", 2, 30),
                at("America/New_York", "2026-03-07T07:00")
            ),
            Some(at("America/New_York", "2026-03-08T03:30"))
        );
        // 2026-11-01 01:30 occurs twice; it fires once, at the earlier offset.
        assert_eq!(
            next_occurrence(
                &daily("America/New_York", 1, 30),
                at("America/New_York", "2026-10-31T08:00")
            ),
            Some(at("America/New_York", "2026-11-01T01:30"))
        );
    }

    #[test]
    fn an_unresolvable_timezone_idles_the_routine() {
        assert_eq!(
            next_occurrence(&daily("Not/AZone", 9, 0), at("UTC", "2026-08-07T00:00")),
            None
        );
    }

    #[test]
    fn non_calendar_arms_are_not_handled_here() {
        assert_eq!(
            next_occurrence(&RoutineSchedule::Manual(ManualSchedule {}), 1_000),
            None
        );
        assert_eq!(
            next_occurrence(
                &RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
                1_000
            ),
            None
        );
        assert_eq!(
            next_occurrence(&RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 }), 1_000),
            None
        );
    }
}
