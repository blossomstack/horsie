# Routine Recurrence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Google-Calendar-style recurring triggers to routines — daily, weekly (chosen weekdays), monthly (day of month), yearly (month + day) — each at a wall-clock time in a per-routine IANA timezone, replacing the three typed schedule columns with one JSON column.

**Architecture:** Extend the fluorite `RoutineSchedule` wire union with four calendar arms; store the serialized union verbatim in a new `schedule` JSON column (deleting the parallel storage `Schedule` enum); compute next-run instants with `jiff` (bundled tzdb) in a new pure `recurrence` module; keep the scheduler's claim-before-run 15s tick untouched. Web form gains calendar-style controls with the browser timezone as default.

**Tech Stack:** Rust (tokio, sqlx, serde_json, jiff 0.2), fluorite IDL codegen, TypeScript/React (no new npm deps — `Intl` APIs only), SQLite + PostgreSQL migrations.

**Spec:** `docs/superpowers/specs/2026-08-07-routine-recurrence-design.md`

## Global Constraints

- **Worktree:** all work happens in `.horsie/worktrees/routine-recurrence` (branch `feat/routine-recurrence`), already created from `origin/main`.
- **jiff** = `"0.2"` with default features (std + bundled timezone db + serde). It is already in `Cargo.lock` at 0.2.28 as a transitive dep; adding a direct server dep does not change the lock.
- **fluorite TS generation** must use the locally installed CLI `fluorite` v0.6.2 (exactly what CI pins). Regenerate BOTH `clients/web/src/generated` (`bun run generate-types`) and `clients/ts/src/generated` (`npm run generate-types`), and commit the output.
- **Migrations** must be added to BOTH `server/migrations/sqlite/` and `server/migrations/postgres/` as `0026_*.sql` (the `migrations_are_in_parity` test and CI enforce it). Legacy `manual`/`every`/`once` rows backfill to exact wire-JSON string literals.
- **Production Rust code: no `unwrap`/`expect`/`panic`.** Test modules may use them (the repo's test modules carry the `#[allow(clippy::unwrap_used, ...)]` attribute).
- **No new npm dependencies** in `clients/web` — `bun.lock` must not change. `Intl.supportedValuesOf("timeZone")` + `Intl.DateTimeFormat().resolvedOptions().timeZone` only.
- **Verification commands** (CI parity): `cargo fmt --all -- --check` (stable toolchain, never nightly), `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --workspace --all-features`, `cd clients/web && bun run test:unit`, `cd clients/web && bun run build`. Tests touching storage run on SQLite locally and PostgreSQL in CI via `db::testing::db()`.
- **Server compile gotcha:** `cargo test --workspace` is the canonical command (single-crate `-p` tests fail on feature gating); use it for all Rust test runs.

---

### Task 1: Wire model — calendar arms in `routines.fl` + regenerate TS

**Files:**
- Modify: `models/fluorite/routines.fl`
- Regenerate: `clients/web/src/generated/routines/*`, `clients/ts/src/generated/routines/*` (committed)

**Interfaces:**
- Produces: `Weekday` enum (`Mon`..`Sun`), structs `DailySchedule { timezone: String, hour: u8, minute: u8 }`, `WeeklySchedule { timezone: String, hour: u8, minute: u8, weekdays: Vec<Weekday> }`, `MonthlySchedule { timezone: String, hour: u8, minute: u8, day_of_month: u8 }`, `YearlySchedule { timezone: String, hour: u8, minute: u8, month: u8, day_of_month: u8 }`, and `RoutineSchedule` gaining `Daily(DailySchedule)`, `Weekly(WeeklySchedule)`, `Monthly(MonthlySchedule)`, `Yearly(YearlySchedule)` — as `horsie_models::routines::{...}` in Rust and `clients/{web,ts}/src/generated/routines/*` in TS. TS payload field names are camelCase (`dayOfMonth`, `weekdays`).

- [ ] **Step 1: Add the calendar arms to the fluorite schema**

Insert between the `OnceSchedule` struct and the `RoutineSchedule` union in `models/fluorite/routines.fl`:

```fluorite
/// Day of the week, Mon first — the order the UI renders.
enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

/// Every day at `hour:minute` in `timezone`.
struct DailySchedule { timezone: String, hour: u8, minute: u8 }

/// On the listed weekdays at `hour:minute` in `timezone`. At least one day;
/// duplicates are rejected at save.
struct WeeklySchedule {
    timezone: String,
    hour: u8,
    minute: u8,
    weekdays: Vec<Weekday>,
}

/// On `day_of_month` of every month in `timezone`. Months without that day
/// (the 31st, the 29th–31st in February) are skipped entirely.
struct MonthlySchedule { timezone: String, hour: u8, minute: u8, day_of_month: u8 }

/// On `month`/`day_of_month` every year in `timezone`. Invalid dates
/// (Feb 29 in a non-leap year) recur only when valid.
struct YearlySchedule { timezone: String, hour: u8, minute: u8, month: u8, day_of_month: u8 }
```

Then extend the union:

```fluorite
union RoutineSchedule {
    Manual(ManualSchedule),
    Every(EverySchedule),
    Once(OnceSchedule),
    Daily(DailySchedule),
    Weekly(WeeklySchedule),
    Monthly(MonthlySchedule),
    Yearly(YearlySchedule),
}
```

- [ ] **Step 2: Regenerate the TypeScript types (both packages)**

```bash
cd clients/web && bun run generate-types
cd clients/ts && npm install --no-audit --no-fund && npm run generate-types
```

If `npm install` is not needed (node_modules present), skip it.

- [ ] **Step 3: Typecheck both packages**

```bash
cd clients/web && bun run typecheck
cd clients/ts && npm run typecheck
```

Expected: clean; new files appear under both `src/generated/routines/` (`weekday.ts`, `dailySchedule.ts`, `weeklySchedule.ts`, `monthlySchedule.ts`, `yearlySchedule.ts`), `routineSchedule.ts` lists all seven arms.

- [ ] **Step 4: Commit**

```bash
git add models/fluorite/routines.fl clients/web/src/generated clients/ts/src/generated
git commit -m "feat: add daily/weekly/monthly/yearly routine schedules to the wire model"
```

---

### Task 2: Server — `recurrence` module with jiff

**Files:**
- Modify: `server/Cargo.toml` (add jiff)
- Create: `server/src/routines/recurrence.rs`
- Modify: `server/src/routines/mod.rs` (declare `pub mod recurrence;`)

**Interfaces:**
- Consumes: `horsie_models::routines::{RoutineSchedule, DailySchedule, WeeklySchedule, MonthlySchedule, YearlySchedule, Weekday}` (Task 1).
- Produces: `pub fn next_occurrence(schedule: &RoutineSchedule, now_ms: u64) -> Option<u64>` — first firing instant strictly after `now_ms`, or `None` for the non-calendar arms and unresolvable timezones. Task 4's `next_run_at` delegates to it.

- [ ] **Step 1: Add jiff as a direct server dependency**

In `server/Cargo.toml` `[dependencies]`, after the `jsonwebtoken` line:

```toml
jiff              = "0.2"
```

- [ ] **Step 2: Write the failing recurrence tests**

Create `server/src/routines/recurrence.rs` with the tests first. Helper for building timestamps:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::routines::{DailySchedule, MonthlySchedule, WeeklySchedule, Weekday, YearlySchedule};

    /// `civil` wall-clock string ("2026-08-07T09:00") in `zone`, as epoch millis.
    fn at(zone: &str, civil: &str) -> u64 {
        let dt: jiff::civil::DateTime = civil.parse().unwrap();
        dt.to_zoned(jiff::tz::TimeZone::get(zone).unwrap())
            .unwrap()
            .timestamp()
            .as_millisecond() as u64
    }

    fn daily(zone: &str, hour: u8, minute: u8) -> RoutineSchedule {
        RoutineSchedule::Daily(DailySchedule { timezone: zone.into(), hour, minute })
    }

    fn weekly(zone: &str, hour: u8, minute: u8, weekdays: &[Weekday]) -> RoutineSchedule {
        RoutineSchedule::Weekly(WeeklySchedule {
            timezone: zone.into(),
            hour,
            minute,
            weekdays: weekdays.to_vec(),
        })
    }

    fn monthly(zone: &str, hour: u8, minute: u8, day_of_month: u8) -> RoutineSchedule {
        RoutineSchedule::Monthly(MonthlySchedule { timezone: zone.into(), hour, minute, day_of_month })
    }

    fn yearly(zone: &str, hour: u8, minute: u8, month: u8, day_of_month: u8) -> RoutineSchedule {
        RoutineSchedule::Yearly(YearlySchedule { timezone: zone.into(), hour, minute, month, day_of_month })
    }

    #[test]
    fn daily_fires_today_if_ahead_else_tomorrow() {
        // 2026-08-07 is a Friday. EDT is UTC-4 in August.
        assert_eq!(
            next_occurrence(&daily("America/New_York", 9, 0), at("America/New_York", "2026-08-07T06:00")),
            Some(at("America/New_York", "2026-08-07T09:00"))
        );
        assert_eq!(
            next_occurrence(&daily("America/New_York", 9, 0), at("America/New_York", "2026-08-07T10:00")),
            Some(at("America/New_York", "2026-08-08T09:00"))
        );
    }

    #[test]
    fn weekly_fires_on_the_next_matching_weekday_and_wraps() {
        let mwf = weekly("Asia/Shanghai", 9, 0, &[Weekday::Mon, Weekday::Wed, Weekday::Fri]);
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
            next_occurrence(&daily("America/New_York", 2, 30), at("America/New_York", "2026-03-07T07:00")),
            Some(at("America/New_York", "2026-03-08T03:30"))
        );
        // 2026-11-01 01:30 occurs twice; it fires once, at the earlier offset.
        assert_eq!(
            next_occurrence(&daily("America/New_York", 1, 30), at("America/New_York", "2026-10-31T08:00")),
            Some(at("America/New_York", "2026-11-01T01:30"))
        );
    }

    #[test]
    fn an_unresolvable_timezone_idles_the_routine() {
        assert_eq!(next_occurrence(&daily("Not/AZone", 9, 0), at("UTC", "2026-08-07T00:00")), None);
    }

    #[test]
    fn non_calendar_arms_are_not_handled_here() {
        use horsie_models::routines::{EverySchedule, ManualSchedule, OnceSchedule};
        assert_eq!(next_occurrence(&RoutineSchedule::Manual(ManualSchedule {}), 1_000), None);
        assert_eq!(
            next_occurrence(&RoutineSchedule::Every(EverySchedule { interval_secs: 60 }), 1_000),
            None
        );
        assert_eq!(
            next_occurrence(&RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 }), 1_000),
            None
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail (module doesn't exist yet)**

```bash
cargo test --workspace recurrence 2>&1 | tail -5
```

Expected: error `unresolved import crate::routines::recurrence` or `file not found`.

- [ ] **Step 4: Implement `recurrence.rs`**

Create `server/src/routines/recurrence.rs`:

```rust
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
    hour: u8,
    minute: u8,
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
```

(Add the `#[cfg(test)] mod tests` block from Step 2 below the code.)

In `server/src/routines/mod.rs`, add `pub mod recurrence;` (check the existing module list first and place it in order).

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --workspace recurrence
```

Expected: all 7 recurrence tests pass.

- [ ] **Step 6: Commit**

```bash
git add server/Cargo.toml server/src/routines/recurrence.rs server/src/routines/mod.rs
git commit -m "feat: next-occurrence math for calendar routine schedules (jiff)"
```

---

### Task 3: Server — migration 0026 (JSON schedule column, both dialects)

**Files:**
- Create: `server/migrations/sqlite/0026_routine_schedule_json.sql`
- Create: `server/migrations/postgres/0026_routine_schedule_json.sql`
- Modify: `server/src/routines/store.rs` (add the migration backfill test)

**Interfaces:**
- Produces: `routines.schedule TEXT` column (wire JSON) replacing `schedule_kind`/`interval_secs`/`at_ms`. Task 4's store reads/writes it.

- [ ] **Step 1: Write the SQLite migration**

`server/migrations/sqlite/0026_routine_schedule_json.sql`:

```sql
-- Routines: the trigger moves from three typed columns to one JSON column
-- holding the serialized `RoutineSchedule` wire union (adjacently tagged,
-- camelCase payloads). The backfill is exact string literals because we own
-- the wire shape, and the old columns always carried their payload for the
-- kind that needed it (the service enforced that at save). DROP COLUMN is
-- fine here: none of the dropped columns is indexed or has a default.
ALTER TABLE routines ADD COLUMN schedule TEXT;

UPDATE routines SET schedule = CASE schedule_kind
    WHEN 'manual' THEN '{"type":"Manual","value":{}}'
    WHEN 'every'  THEN '{"type":"Every","value":{"intervalSecs":' || interval_secs || '}}'
    WHEN 'once'   THEN '{"type":"Once","value":{"atMs":' || at_ms || '}}'
END;

ALTER TABLE routines DROP COLUMN schedule_kind;
ALTER TABLE routines DROP COLUMN interval_secs;
ALTER TABLE routines DROP COLUMN at_ms;
```

- [ ] **Step 2: Write the PostgreSQL mirror**

`server/migrations/postgres/0026_routine_schedule_json.sql`:

```sql
-- PostgreSQL mirror of migrations/sqlite/0026_routine_schedule_json.sql.
-- Same shape; PostgreSQL combines the three drops into one statement.
ALTER TABLE routines ADD COLUMN schedule TEXT;

UPDATE routines SET schedule = CASE schedule_kind
    WHEN 'manual' THEN '{"type":"Manual","value":{}}'
    WHEN 'every'  THEN '{"type":"Every","value":{"intervalSecs":' || interval_secs || '}}'
    WHEN 'once'   THEN '{"type":"Once","value":{"atMs":' || at_ms || '}}'
END;

ALTER TABLE routines DROP COLUMN schedule_kind,
                     DROP COLUMN interval_secs,
                     DROP COLUMN at_ms;
```

- [ ] **Step 3: Write the failing migration test**

Append to the tests module in `server/src/routines/store.rs` (pattern: `migration_0006_drops_api_key_env_and_preserves_rows` in `server/src/config/store.rs`):

```rust
/// SQLite-only, like the 0006 test: it builds the pre-0026 schema by hand and
/// then applies exactly 0026, pinning the backfill to the wire JSON shape.
#[tokio::test]
async fn migration_0026_backfills_schedule_json_and_drops_the_typed_columns() {
    let pool = &crate::db::testing::unmigrated_sqlite().await;

    // The post-0024 `routines` shape: scoped, with the three schedule columns.
    sqlx::query(
        "CREATE TABLE routines (
            user_id         TEXT    NOT NULL,
            name            TEXT    NOT NULL,
            description     TEXT    NOT NULL DEFAULT '',
            agent           TEXT    NOT NULL,
            prompt          TEXT    NOT NULL,
            schedule_kind   TEXT    NOT NULL,
            interval_secs   INTEGER,
            at_ms           INTEGER,
            enabled         INTEGER NOT NULL DEFAULT 1,
            next_run_at_ms  INTEGER,
            last_run_at_ms  INTEGER,
            last_session_id TEXT,
            last_error      TEXT,
            created_at      TEXT    NOT NULL,
            updated_at      TEXT    NOT NULL,
            PRIMARY KEY (user_id, name)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routines (user_id, name, description, agent, prompt, schedule_kind, \
         interval_secs, at_ms, enabled, next_run_at_ms, created_at, updated_at) VALUES \
         ('1', 'manual', '', 'a', 'p', 'manual', NULL, NULL, 1, NULL, '1', '1'), \
         ('1', 'hourly', '', 'a', 'p', 'every', 3600, NULL, 1, 3601000, '1', '1'), \
         ('1', 'launch', '', 'a', 'p', 'once', NULL, 5000, 0, NULL, '1', '1')",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(include_str!(
        "../../migrations/sqlite/0026_routine_schedule_json.sql"
    ))
    .execute(pool)
    .await
    .unwrap();

    let rows = sqlx::query("SELECT name, schedule, enabled, next_run_at_ms FROM routines")
        .fetch_all(pool)
        .await
        .unwrap();
    let got: HashMap<String, (String, i64, Option<i64>)> = rows
        .iter()
        .map(|r| {
            (
                r.try_get::<String, _>("name").unwrap(),
                (
                    r.try_get::<String, _>("schedule").unwrap(),
                    r.try_get::<i64, _>("enabled").unwrap(),
                    r.try_get::<Option<i64>, _>("next_run_at_ms").unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(
        got.get("manual").unwrap().0,
        r#"{"type":"Manual","value":{}}"#
    );
    assert_eq!(
        got.get("hourly").unwrap().0,
        r#"{"type":"Every","value":{"intervalSecs":3600}}"#
    );
    assert_eq!(
        got.get("launch").unwrap().0,
        r#"{"type":"Once","value":{"atMs":5000}}"#
    );
    assert_eq!(got.get("hourly").unwrap().1, 1);
    assert_eq!(got.get("hourly").unwrap().2, Some(3_601_000));
    assert_eq!(got.get("launch").unwrap().1, 0, "enabled survives");

    let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('routines')")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<String, _>("name").unwrap())
        .collect();
    for dropped in ["schedule_kind", "interval_secs", "at_ms"] {
        assert!(!cols.iter().any(|c| c == dropped), "{dropped} still present");
    }
}
```

(Add `use std::collections::HashMap;` to the store.rs test module if not already imported.)

- [ ] **Step 4: Run the test to verify it fails (migration file missing)**

```bash
cargo test --workspace migration_0026 2>&1 | tail -5
```

Expected: fail — `0026_routine_schedule_json.sql` not found / include_str error.

- [ ] **Step 5: Run it again to verify it passes** (files now exist)

```bash
cargo test --workspace migration_0026
```

Expected: pass. Also confirm both dialects are in parity: `cargo test --workspace migrations_are_in_parity`.

- [ ] **Step 6: Commit**

```bash
git add server/migrations/sqlite/0026_routine_schedule_json.sql server/migrations/postgres/0026_routine_schedule_json.sql server/src/routines/store.rs
git commit -m "feat: migrate routines schedule to a single JSON column"
```

---

### Task 4: Server — store & service switch to the wire schedule

**Files:**
- Modify: `server/src/routines/store.rs`
- Modify: `server/src/routines/service.rs`
- Modify: `server/src/routines/scheduler.rs` (tests only)
- Verify: `server/src/routines/runner.rs` compiles (no change expected)

**Interfaces:**
- Consumes: wire `RoutineSchedule` from Task 1; `recurrence::next_occurrence` from Task 2; `routines.schedule` column from Task 3.
- Produces: `RoutineRow.schedule: RoutineSchedule`; `pub fn next_run_at(schedule: &RoutineSchedule, enabled: bool, now_ms: u64) -> Option<u64>` in `service.rs`; save-time validation for all seven arms.
- Deletes: the storage `Schedule` enum, `Schedule::from_columns`, `storage_schedule`, `wire_schedule`.

- [ ] **Step 1: Switch `store.rs` to the wire schedule type**

In `server/src/routines/store.rs`:

1. Delete the `Schedule` enum and its `impl Schedule` block (kind/interval_secs/at_ms/from_columns).
2. Add `use horsie_models::routines::RoutineSchedule;` at the top.
3. `RoutineRow.schedule: RoutineSchedule` (replace `pub schedule: Schedule`).
4. `const COLS` becomes:

```rust
const COLS: &str = "name, description, agent, prompt, schedule, enabled, next_run_at_ms, \
                    last_run_at_ms, last_session_id, last_error, created_at, updated_at";
```

5. `insert` — 13 placeholders (`user_id` + 12 COLS) and bind the JSON:

```rust
.bind(serde_json::to_string(&row.schedule).map_err(|e| e.to_string())?)
```

(replacing the three `.bind(row.schedule.kind())` / `.interval_secs()` / `.at_ms()` binds).

6. `replace` — update the statement to `SET description = ?, agent = ?, prompt = ?, schedule = ?, enabled = ?, next_run_at_ms = ?, updated_at = ?` and bind JSON the same way.

7. `row_to_routine` — parse:

```rust
schedule: serde_json::from_str(&get("schedule")?)
    .map_err(|e| format!("routines.schedule: {e}"))?,
```

- [ ] **Step 2: Update the store tests**

In the store tests module:

- `row(name, schedule)` helper takes `RoutineSchedule`.
- `every_schedule_shape_round_trips` loops all **seven** arms: Manual, Every, Once, Daily, Weekly, Monthly, Yearly (build literals; see the recurrence test module for the struct shapes).
- `replace_swaps_the_definition_and_keeps_run_history` uses `RoutineSchedule::Every(EverySchedule { interval_secs: 60 })`.
- `due_respects_...`, `record_run_...`, `arm_moves_...`, `using_agent_...` — change `Schedule::Every { interval_secs: 60 }` to the wire `RoutineSchedule::Every(EverySchedule { interval_secs: 60 })`; `Schedule::Manual` to `RoutineSchedule::Manual(ManualSchedule {})`; update the `use horsie_models::routines::{...}` import accordingly.
- Replace `a_schedule_row_missing_its_payload_is_an_error` with an invalid-JSON test:

```rust
#[tokio::test]
async fn a_schedule_row_with_invalid_json_is_an_error() {
    // Not a silently-defaulted schedule: a routine running at some other
    // cadence than the one it was saved with is worse than a load failure.
    let (s, _db) = store().await;
    s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
        .await
        .unwrap();
    sqlx::query("UPDATE routines SET schedule = '{broken' WHERE name = 'a'")
        .execute(_db.pool())
        .await
        .unwrap();
    let err = s.get("a").await.unwrap_err();
    assert!(err.contains("schedule"), "{err}");
}
```

- [ ] **Step 3: Run the store tests — expect failures at this point**

```bash
cargo test --workspace routines::store 2>&1 | tail -20
```

Expected: compile errors — `service.rs` still references the deleted `Schedule` (proceed to Step 4; the crate is mid-switch).

- [ ] **Step 4: Switch `service.rs`**

In `server/src/routines/service.rs`:

1. Imports become:

```rust
use horsie_models::routines::{
    EverySchedule, ManualSchedule, MonthlySchedule, RoutineInput, RoutineSchedule, Weekday,
    WeeklySchedule, DailySchedule, YearlySchedule,
};
use jiff::tz::TimeZone;
```

2. `next_run_at` — signature and body:

```rust
/// When a schedule should next fire, given the moment it was armed.
///
/// `Every` is measured from `now`, not from a fixed origin: a server that was
/// down for a day resumes with one run rather than a day of backlog. `Once`
/// only fires if its instant is still ahead; a paused routine never fires.
/// The calendar arms delegate to [`crate::routines::recurrence::next_occurrence`].
pub fn next_run_at(schedule: &RoutineSchedule, enabled: bool, now_ms: u64) -> Option<u64> {
    if !enabled {
        return None;
    }
    match schedule {
        RoutineSchedule::Manual(_) => None,
        RoutineSchedule::Every(e) => Some(now_ms.saturating_add(e.interval_secs * 1_000)),
        RoutineSchedule::Once(o) => (o.at_ms > now_ms).then_some(o.at_ms),
        s @ (RoutineSchedule::Daily(_)
        | RoutineSchedule::Weekly(_)
        | RoutineSchedule::Monthly(_)
        | RoutineSchedule::Yearly(_)) => crate::routines::recurrence::next_occurrence(s, now_ms),
    }
}
```

3. `validate` — replace the interval-only check with a match over the resolved wire schedule:

```rust
let schedule = input.schedule.clone().unwrap_or(RoutineSchedule::Manual(ManualSchedule {}));
validate_schedule(&schedule)?;
Ok((schedule, input.enabled.unwrap_or(true)))
```

4. Add `validate_schedule`:

```rust
/// Save-time validation for a schedule. Covers what is stable at save: the
/// interval floor, the IANA timezone name, and that the calendar fields are
/// in range. The agent's own contents are live state, re-checked by the
/// runner at every trigger.
fn validate_schedule(schedule: &RoutineSchedule) -> Result<(), RoutineError> {
    match schedule {
        RoutineSchedule::Every(e) if e.interval_secs < MIN_INTERVAL_SECS => Err(
            RoutineError::Invalid(format!(
                "interval must be at least {MIN_INTERVAL_SECS} seconds"
            )),
        ),
        RoutineSchedule::Daily(d) => validate_clock(&d.timezone, d.hour, d.minute),
        RoutineSchedule::Weekly(w) => {
            validate_clock(&w.timezone, w.hour, w.minute)?;
            if w.weekdays.is_empty() {
                return Err(RoutineError::Invalid(
                    "weekly schedule needs at least one weekday".to_string(),
                ));
            }
            if w.weekdays.windows(2).any(|p| weekday_rank(&p[0]) >= weekday_rank(&p[1])) {
                return Err(RoutineError::Invalid(
                    "weekdays must be unique and in Mon–Sun order".to_string(),
                ));
            }
            Ok(())
        }
        RoutineSchedule::Monthly(m) => {
            validate_clock(&m.timezone, m.hour, m.minute)?;
            validate_day_of_month(m.day_of_month)
        }
        RoutineSchedule::Yearly(y) => {
            validate_clock(&y.timezone, y.hour, y.minute)?;
            if !(1..=12).contains(&y.month) {
                return Err(RoutineError::Invalid(format!("month must be 1–12, got {}", y.month)));
            }
            validate_day_of_month(y.day_of_month)
        }
        RoutineSchedule::Manual(_) | RoutineSchedule::Every(_) | RoutineSchedule::Once(_) => Ok(()),
    }
}

fn validate_clock(timezone: &str, hour: u8, minute: u8) -> Result<(), RoutineError> {
    TimeZone::get(timezone).map_err(|_| {
        RoutineError::Invalid(format!("unknown timezone '{timezone}'"))
    })?;
    if hour > 23 {
        return Err(RoutineError::Invalid(format!("hour must be 0–23, got {hour}")));
    }
    if minute > 59 {
        return Err(RoutineError::Invalid(format!("minute must be 0–59, got {minute}")));
    }
    Ok(())
}

fn validate_day_of_month(day: u8) -> Result<(), RoutineError> {
    if !(1..=31).contains(&day) {
        return Err(RoutineError::Invalid(format!(
            "day of month must be 1–31, got {day}"
        )));
    }
    Ok(())
}

fn weekday_rank(d: &Weekday) -> u8 {
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
```

5. Delete `storage_schedule` and `wire_schedule`; `row_from_input` now takes the wire schedule straight from the input:

```rust
fn row_from_input(
    input: RoutineInput,
    schedule: RoutineSchedule,
    enabled: bool,
    now_ms: u64,
    created_at: String,
    updated_at: String,
) -> RoutineRow {
    RoutineRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        agent: input.agent,
        prompt: input.prompt,
        next_run_at_ms: next_run_at(&schedule, enabled, now_ms),
        schedule,
        enabled,
        // Never carried from the input: run history belongs to the runs.
        last_run_at_ms: None,
        last_session_id: None,
        last_error: None,
        created_at,
        updated_at,
    }
}
```

6. `routine_view` — replace `wire_schedule(&row.schedule)` with `row.schedule.clone()`.

- [ ] **Step 5: Update the service tests**

- `next_run_measures_an_interval_from_now_and_never_re_arms_a_once` — signature takes `&RoutineSchedule` now; rewrite as `next_run_at(&RoutineSchedule::Manual(ManualSchedule {}), true, 1_000)` etc. Add calendar-arm cases:

```rust
assert_eq!(
    next_run_at(
        &RoutineSchedule::Daily(DailySchedule { timezone: "UTC".into(), hour: 9, minute: 0 }),
        true,
        1_000, // 1970-01-01T00:00:01Z — Thursday
    ),
    Some(1_000 + 9 * 3_600 * 1_000),
    "09:00 UTC on 1970-01-01 is 8h59m59s away"
);
assert_eq!(
    next_run_at(&RoutineSchedule::Daily(DailySchedule { timezone: "UTC".into(), hour: 9, minute: 0 }), false, 1_000),
    None,
    "a paused calendar routine never fires"
);
```

- `create_arms_a_recurring_schedule_from_now` — unchanged (Every).
- `create_validates_the_slug_prompt_agent_and_interval` — add validation rejections:

```rust
let bad_zone = input("a", Some(RoutineSchedule::Daily(DailySchedule {
    timezone: "Not/AZone".into(), hour: 9, minute: 0,
})));
assert!(matches!(
    s.create(bad_zone, 0).await.unwrap_err(),
    RoutineError::Invalid(m) if m.contains("timezone")
));

let bad_hour = input("a", Some(RoutineSchedule::Daily(DailySchedule {
    timezone: "UTC".into(), hour: 24, minute: 0,
})));
assert!(matches!(
    s.create(bad_hour, 0).await.unwrap_err(),
    RoutineError::Invalid(m) if m.contains("hour")
));

let no_days = input("a", Some(RoutineSchedule::Weekly(WeeklySchedule {
    timezone: "UTC".into(), hour: 9, minute: 0, weekdays: vec![],
})));
assert!(matches!(
    s.create(no_days, 0).await.unwrap_err(),
    RoutineError::Invalid(m) if m.contains("weekday")
));

let dup_days = input("a", Some(RoutineSchedule::Weekly(WeeklySchedule {
    timezone: "UTC".into(), hour: 9, minute: 0,
    weekdays: vec![Weekday::Mon, Weekday::Mon],
})));
assert!(matches!(
    s.create(dup_days, 0).await.unwrap_err(),
    RoutineError::Invalid(_)
));

let bad_month_day = input("a", Some(RoutineSchedule::Monthly(MonthlySchedule {
    timezone: "UTC".into(), hour: 9, minute: 0, day_of_month: 32,
})));
assert!(matches!(
    s.create(bad_month_day, 0).await.unwrap_err(),
    RoutineError::Invalid(m) if m.contains("day of month")
));

let bad_year = input("a", Some(RoutineSchedule::Yearly(YearlySchedule {
    timezone: "UTC".into(), hour: 9, minute: 0, month: 13, day_of_month: 1,
})));
assert!(matches!(
    s.create(bad_year, 0).await.unwrap_err(),
    RoutineError::Invalid(m) if m.contains("month")
));
```

- Add a happy-path create for a weekly schedule arming to the next occurrence:

```rust
#[tokio::test]
async fn create_arms_a_weekly_schedule_to_its_next_weekday() {
    let (s, _t) = service().await;
    // now = 1970-01-01T00:00:01Z, a Thursday; Mon/Wed/Fri 09:00 → Friday 09:00.
    let v = s
        .create(
            input(
                "triages",
                Some(RoutineSchedule::Weekly(WeeklySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                    weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                })),
            ),
            1_000,
        )
        .await
        .unwrap();
    assert_eq!(v.next_run_at_ms, Some(1_000 + 24 * 3_600 * 1_000 + 9 * 3_600 * 1_000));
}
```

- [ ] **Step 6: Fix the scheduler and its tests**

- `server/src/routines/scheduler.rs` — no production change (it calls `service::next_run_at` by signature); the test module's `use` line stays on `horsie_models::routines::{EverySchedule, OnceSchedule, RoutineInput, RoutineSchedule}` (unchanged names).
- Add a weekly scheduler test (extends the existing pattern):

```rust
#[tokio::test]
async fn a_weekly_routine_fires_once_due_and_re_arms_to_the_next_weekday() {
    let tmp = tempfile::tempdir().unwrap();
    let db = crate::db::testing::db().await;
    let users = registry(db.clone(), &tmp);
    let a = account(&users, &UserId::bootstrap(), true).await;
    let scheduler = RoutineScheduler::new(db, users);

    // 1970-01-01T00:00:01Z is a Thursday; Mon/Wed/Fri 09:00 UTC → Friday 09:00.
    a.services
        .routines
        .create(
            routine(
                "triages",
                RoutineSchedule::Weekly(WeeklySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                    weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                }),
            ),
            1_000,
        )
        .await
        .unwrap();
    let first = a.services.routines.get("triages").await.unwrap().next_run_at_ms;
    assert_eq!(first, Some(1_000 + 24 * 3_600 * 1_000 + 9 * 3_600 * 1_000));

    scheduler.tick(first.unwrap() - 1).await;
    assert!(sessions(&a.services.supervisor).await.is_empty(), "not due yet");

    scheduler.tick(first.unwrap()).await;
    assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
    let view = a.services.routines.get("triages").await.unwrap();
    // Friday 09:00 fired → re-arms to Monday 09:00 (3 days later).
    assert_eq!(
        view.next_run_at_ms,
        Some(first.unwrap() + 3 * 24 * 3_600 * 1_000)
    );

    scheduler.tick(first.unwrap()).await;
    assert_eq!(sessions(&a.services.supervisor).await.len(), 1, "no double fire");
}
```

(Add `WeeklySchedule` and `Weekday` to the scheduler test module's `use horsie_models::routines::{...}` import.)

- [ ] **Step 7: Run the full workspace test suite**

```bash
cargo test --workspace
```

Expected: all pass, including the new store/service/scheduler/recurrence tests. If `runner.rs` fails to compile, check whether it referenced `Schedule` (it should not — it uses `row.prompt`/`row.agent`; its test module uses wire types already).

- [ ] **Step 8: Commit**

```bash
git add server/src/routines
git commit -m "feat: store and validate calendar schedules as wire JSON"
```

---

### Task 5: CLI — schedule labels for the new arms

**Files:**
- Modify: `cli/src/routines.rs`

**Interfaces:**
- Consumes: wire `RoutineSchedule` (Task 1) — the CLI already imports `horsie_models::routines::{RoutineRunResponse, RoutineSchedule, RoutineView}`.
- Produces: `schedule_label(&RoutineSchedule) -> String` covering all seven arms.

- [ ] **Step 1: Write the failing label tests**

In `cli/src/routines.rs` tests, extend `schedule_label_covers_all_arms`:

```rust
assert_eq!(
    schedule_label(&RoutineSchedule::Daily(DailySchedule {
        timezone: "Asia/Shanghai".into(),
        hour: 9,
        minute: 5,
    })),
    "daily 09:05 Asia/Shanghai"
);
assert_eq!(
    schedule_label(&RoutineSchedule::Weekly(WeeklySchedule {
        timezone: "Asia/Shanghai".into(),
        hour: 9,
        minute: 0,
        weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
    })),
    "weekly mon,wed,fri 09:00 Asia/Shanghai"
);
assert_eq!(
    schedule_label(&RoutineSchedule::Monthly(MonthlySchedule {
        timezone: "UTC".into(),
        hour: 9,
        minute: 0,
        day_of_month: 15,
    })),
    "monthly 15th 09:00 UTC"
);
assert_eq!(
    schedule_label(&RoutineSchedule::Yearly(YearlySchedule {
        timezone: "UTC".into(),
        hour: 9,
        minute: 0,
        month: 2,
        day_of_month: 15,
    })),
    "yearly feb-15 09:00 UTC"
);
```

(Update the `use horsie_models::routines::{...}` import in the tests to include the new types.)

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --workspace cli::routines 2>&1 | tail -5
```

Expected: compile error — the match in `schedule_label` is not exhaustive over the new arms.

- [ ] **Step 3: Implement the labels**

In `schedule_label`, add:

```rust
RoutineSchedule::Daily(d) => format!("daily {:02}:{:02} {}", d.hour, d.minute, d.timezone),
RoutineSchedule::Weekly(w) => {
    let days = w.weekdays.iter().map(weekday_abbr).collect::<Vec<_>>().join(",");
    format!("weekly {days} {:02}:{:02} {}", w.hour, w.minute, w.timezone)
}
RoutineSchedule::Monthly(m) => {
    format!("monthly {}th {:02}:{:02} {}", m.day_of_month, m.hour, m.minute, m.timezone)
}
RoutineSchedule::Yearly(y) => format!(
    "yearly {}-{} {:02}:{:02} {}",
    month_abbr(y.month),
    y.day_of_month,
    y.hour,
    y.minute,
    y.timezone
),
```

And helpers (module-private, above `schedule_label`):

```rust
fn weekday_abbr(d: &Weekday) -> &'static str {
    match d {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

fn month_abbr(month: u8) -> &'static str {
    [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ][(month - 1) as usize]
}
```

Add `use horsie_models::routines::{DailySchedule, MonthlySchedule, Weekday, WeeklySchedule, YearlySchedule};` to the imports (or fold into the existing routines import).

- [ ] **Step 4: Run the tests**

```bash
cargo test --workspace cli::routines
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add cli/src/routines.rs
git commit -m "feat: render calendar routine schedules in the CLI"
```

---

### Task 6: Web UI — form controls and schedule descriptions

**Files:**
- Modify: `clients/web/src/lib/schedule.ts`
- Modify: `clients/web/src/pages/routines/RoutineEditPage.tsx`
- Create: `clients/web/src/lib/schedule.test.ts`
- Create: `clients/web/src/pages/routines/RoutineEditPage.test.tsx`

**Interfaces:**
- Consumes: generated TS types (Task 1): `RoutineSchedule` arms `Daily/Weekly/Monthly/Yearly` with camelCase `value` shapes, `Weekday` enum with string values `"Mon".."Sun"`.
- Produces: `describeSchedule` covering all arms; `browserTimezone()` and `timezoneOptions()` helpers; edit-form controls keyed by `data-testid`.

- [ ] **Step 1: Write the failing `schedule.ts` tests**

Create `clients/web/src/lib/schedule.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { RoutineSchedule } from "../api/types";
import { describeSchedule } from "./schedule";

function schedule(s: RoutineSchedule) {
  return describeSchedule(s);
}

describe("describeSchedule", () => {
  it("describes the manual arm", () => {
    expect(schedule({ type: "Manual", value: {} })).toBe("manually");
  });

  it("describes every by the coarsest whole unit", () => {
    expect(
      schedule({ type: "Every", value: { intervalSecs: 3600 } }),
    ).toBe("every 1h");
  });

  it("describes once as a local instant", () => {
    const s = schedule({ type: "Once", value: { atMs: 0 } });
    expect(s).toMatch(/^once on /);
  });

  it("describes daily with time and zone", () => {
    expect(
      schedule({
        type: "Daily",
        value: { timezone: "Asia/Shanghai", hour: 9, minute: 5 },
      }),
    ).toBe("daily at 09:05 (Asia/Shanghai)");
  });

  it("describes weekly with its days", () => {
    expect(
      schedule({
        type: "Weekly",
        value: {
          timezone: "Asia/Shanghai",
          hour: 9,
          minute: 0,
          weekdays: ["Mon", "Wed", "Fri"],
        },
      }),
    ).toBe("every Mon, Wed, Fri at 09:00 (Asia/Shanghai)");
  });

  it("describes monthly with an ordinal day", () => {
    expect(
      schedule({
        type: "Monthly",
        value: { timezone: "UTC", hour: 9, minute: 0, dayOfMonth: 15 },
      }),
    ).toBe("monthly on the 15th at 09:00 (UTC)");
  });

  it("describes yearly with month and day", () => {
    expect(
      schedule({
        type: "Yearly",
        value: { timezone: "UTC", hour: 9, minute: 0, month: 2, dayOfMonth: 15 },
      }),
    ).toBe("yearly on Feb 15 at 09:00 (UTC)");
  });
});
```

- [ ] **Step 2: Run to verify failure**

```bash
cd clients/web && bun run test:unit -- schedule.test
```

Expected: fail — the new arms return `undefined`/compile error.

- [ ] **Step 3: Implement `schedule.ts` additions**

Add to `clients/web/src/lib/schedule.ts`:

```ts
const MONTHS = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun",
  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
] as const;

/** "9" → "09", for clock and day rendering. */
function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/** "3" → "3rd"; the ordinal suffix a calendar reader expects. */
function ordinal(n: number): string {
  const last = n % 10;
  const suffix = last === 1 ? "st" : last === 2 ? "nd" : last === 3 ? "rd" : "th";
  return `${n}${suffix}`;
}

/** The browser's IANA timezone; the form's default. */
export function browserTimezone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
}

const FALLBACK_ZONES = [
  "UTC",
  "America/New_York",
  "America/Los_Angeles",
  "Europe/London",
  "Europe/Berlin",
  "Asia/Shanghai",
  "Asia/Tokyo",
  "Australia/Sydney",
];

/** IANA zones for the timezone picker, alphabetized (as the spec requires),
 * with a curated fallback for engines without `Intl.supportedValuesOf`. */
export function timezoneOptions(): string[] {
  const zones = Intl.supportedValuesOf?.("timeZone") ?? [];
  if (zones.length === 0) return FALLBACK_ZONES;
  const all = [...zones];
  if (!all.includes(browserTimezone())) all.push(browserTimezone());
  return all.sort();
}
```

Extend `describeSchedule`:

```ts
case "Daily":
  return `daily at ${pad2(v.hour)}:${pad2(v.minute)} (${v.timezone})`;
case "Weekly":
  return `every ${v.weekdays.join(", ")} at ${pad2(v.hour)}:${pad2(v.minute)} (${v.timezone})`;
case "Monthly":
  return `monthly on the ${ordinal(v.dayOfMonth)} at ${pad2(v.hour)}:${pad2(v.minute)} (${v.timezone})`;
case "Yearly":
  return `yearly on ${MONTHS[v.month - 1]} ${ordinal(v.dayOfMonth)} at ${pad2(v.hour)}:${pad2(v.minute)} (${v.timezone})`;
```

- [ ] **Step 4: Run the schedule tests**

```bash
cd clients/web && bun run test:unit -- schedule.test
```

Expected: pass.

- [ ] **Step 5: Write the failing edit-page test**

Create `clients/web/src/pages/routines/RoutineEditPage.test.tsx`:

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentView, RoutineInput } from "../../api/types";
import { RoutineEditPage } from "./RoutineEditPage";

afterEach(cleanup);

const create = vi.fn(async (body: RoutineInput) => body);

vi.mock("../../api/client", () => ({
  api: {
    agents: {
      list: async (): Promise<AgentView[]> => [
        {
          name: "reviewer",
          description: "",
          model: "sonnet",
          repos: [],
          plugins: [],
          mcpServers: [],
          memorySpaces: [],
          createdAt: "1",
          updatedAt: "1",
        },
      ],
    },
    routines: {
      get: async () => undefined,
      create: (body: RoutineInput) => create(body),
    },
  },
  ApiRequestError: class extends Error {},
}));

function renderNew() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/routines/new"]}>
        <Routes>
          <Route path="/routines/new" element={<RoutineEditPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("RoutineEditPage", () => {
  it("defaults the timezone to the browser's and saves a daily schedule", async () => {
    const { findByTestId, getByTestId } = renderNew();
    fireEvent.change(await findByTestId("routine-name-input"), {
      target: { value: "morning" },
    });
    fireEvent.change(getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "triage the queue" },
    });
    fireEvent.change(getByTestId("routine-schedule-kind"), {
      target: { value: "Daily" },
    });

    const expectedZone = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
    const zone = getByTestId("routine-timezone-select") as HTMLSelectElement;
    expect(zone.value).toBe(expectedZone);

    fireEvent.change(getByTestId("routine-time-input"), {
      target: { value: "09:00" },
    });
    fireEvent.click(getByTestId("save-routine-button"));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(create.mock.calls[0]?.[0].schedule).toEqual({
      type: "Daily",
      value: { timezone: expectedZone, hour: 9, minute: 0 },
    });
  });

  it("weekly requires at least one weekday before saving", async () => {
    const { findByTestId, getByTestId } = renderNew();
    fireEvent.change(await findByTestId("routine-name-input"), {
      target: { value: "standup" },
    });
    fireEvent.change(getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "summarize yesterday" },
    });
    fireEvent.change(getByTestId("routine-schedule-kind"), {
      target: { value: "Weekly" },
    });

    const save = getByTestId("save-routine-button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    fireEvent.click(getByTestId("weekday-mon"));
    expect(save.disabled).toBe(false);

    fireEvent.click(save);
    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    const payload = create.mock.calls[0]?.[0].schedule as { type: string; value: { weekdays: string[]; timezone: string; hour: number; minute: number } };
    expect(payload.type).toBe("Weekly");
    expect(payload.value.weekdays).toEqual(["Mon"]);
    expect(payload.value.hour).toBe(9);
  });
});
```

(Note: the form's time input defaults to `"09:00"`, and the weekday chips are `data-testid="weekday-mon"` etc. — implemented in Step 7.)

- [ ] **Step 6: Run to verify failure**

```bash
cd clients/web && bun run test:unit -- RoutineEditPage
```

Expected: fail — no `routine-timezone-select`/`routine-time-input`/`weekday-mon` testids yet.

- [ ] **Step 7: Implement the edit-form controls**

In `clients/web/src/pages/routines/RoutineEditPage.tsx`:

1. Imports: add `type Weekday` to the `../../api/types` import; add `browserTimezone, timezoneOptions` to the `../../lib/schedule` import.

2. Add state (after the existing `atLocal` state):

```tsx
const [timezone, setTimezone] = useState(
  initial && initial.schedule.type !== "Manual" &&
  initial.schedule.type !== "Every" && initial.schedule.type !== "Once"
    ? initial.schedule.value.timezone
    : browserTimezone(),
);
const [timeOfDay, setTimeOfDay] = useState(
  initial && initial.schedule.type !== "Manual" &&
  initial.schedule.type !== "Every" && initial.schedule.type !== "Once"
    ? `${String(initial.schedule.value.hour).padStart(2, "0")}:${String(initial.schedule.value.minute).padStart(2, "0")}`
    : "09:00",
);
const [weekdays, setWeekdays] = useState<Set<Weekday>>(
  new Set(
    initial?.schedule.type === "Weekly" ? initial.schedule.value.weekdays : [],
  ),
);
const [dayOfMonth, setDayOfMonth] = useState(
  initial?.schedule.type === "Monthly" || initial?.schedule.type === "Yearly"
    ? String(initial.schedule.value.dayOfMonth)
    : "1",
);
const [month, setMonth] = useState(
  initial?.schedule.type === "Yearly" ? initial.schedule.value.month : 1,
);
```

3. Add helpers near `buildSchedule`:

```tsx
const hourOf = (t: string) => Number(t.split(":")[0]);
const minuteOf = (t: string) => Number(t.split(":")[1]);
const timeIsValid = /^\d{2}:\d{2}$/.test(timeOfDay);
const dayValid = Number(dayOfMonth) >= 1 && Number(dayOfMonth) <= 31;
const calendarValid =
  timezone !== "" &&
  timeIsValid &&
  (kind !== "Weekly" || weekdays.size > 0) &&
  (kind !== "Monthly" || dayValid) &&
  (kind !== "Yearly" || (month >= 1 && month <= 12 && dayValid));
```

4. Extend `scheduleValid`:

```tsx
const scheduleValid =
  kind === "Manual" ||
  (kind === "Every" && intervalSecs >= MIN_INTERVAL_SECS) ||
  (kind === "Once" && !Number.isNaN(fromLocalInputValue(atLocal))) ||
  calendarValid;
```

5. Extend `buildSchedule`:

```tsx
case "Daily":
  return {
    type: "Daily",
    value: { timezone, hour: hourOf(timeOfDay), minute: minuteOf(timeOfDay) },
  };
case "Weekly":
  return {
    type: "Weekly",
    value: {
      timezone,
      hour: hourOf(timeOfDay),
      minute: minuteOf(timeOfDay),
      weekdays: WEEKDAY_ORDER.filter((d) => weekdays.has(d)),
    },
  };
case "Monthly":
  return {
    type: "Monthly",
    value: {
      timezone,
      hour: hourOf(timeOfDay),
      minute: minuteOf(timeOfDay),
      dayOfMonth: Number(dayOfMonth),
    },
  };
case "Yearly":
  return {
    type: "Yearly",
    value: {
      timezone,
      hour: hourOf(timeOfDay),
      minute: minuteOf(timeOfDay),
      month,
      dayOfMonth: Number(dayOfMonth),
    },
  };
```

with, at module level:

```tsx
/** The canonical Mon–Sun order the server requires. */
const WEEKDAY_ORDER: Weekday[] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
```

6. Add the dropdown options:

```tsx
<option value="Daily">Daily, at a time</option>
<option value="Weekly">Weekly, on chosen days</option>
<option value="Monthly">Monthly, on a day</option>
<option value="Yearly">Yearly, on a date</option>
```

7. Add the shared calendar controls inside the trigger fieldset, after the existing `{kind === "Once" && ...}` block:

```tsx
{["Daily", "Weekly", "Monthly", "Yearly"].includes(kind) && (
  <>
    <label className="flex items-center gap-2 text-sm text-dim">
      at
      <input
        className="field"
        type="time"
        value={timeOfDay}
        onChange={(e) => setTimeOfDay(e.target.value)}
        data-testid="routine-time-input"
      />
      <select
        className="field"
        value={timezone}
        onChange={(e) => setTimezone(e.target.value)}
        data-testid="routine-timezone-select"
      >
        {timezoneOptions().map((z) => (
          <option key={z} value={z}>
            {z}
          </option>
        ))}
      </select>
    </label>

    {kind === "Weekly" && (
      <div className="flex flex-wrap items-center gap-1">
        {WEEKDAY_ORDER.map((d) => (
          <button
            key={d}
            type="button"
            className={`chip ${weekdays.has(d) ? "chip-on" : ""}`}
            aria-pressed={weekdays.has(d)}
            onClick={() =>
              setWeekdays((prev) => {
                const next = new Set(prev);
                if (next.has(d)) next.delete(d);
                else next.add(d);
                return next;
              })
            }
            data-testid={`weekday-${d.toLowerCase()}`}
          >
            {d}
          </button>
        ))}
        <button
          type="button"
          className="chip"
          onClick={() => setWeekdays(new Set(WEEKDAY_ORDER.slice(0, 5)))}
        >
          Mon–Fri
        </button>
      </div>
    )}

    {kind === "Monthly" && (
      <label className="flex items-center gap-2 text-sm text-dim">
        on the
        <input
          className="field w-20"
          type="number"
          min={1}
          max={31}
          value={dayOfMonth}
          onChange={(e) => setDayOfMonth(e.target.value)}
          data-testid="routine-day-of-month"
        />
        day
      </label>
    )}

    {kind === "Yearly" && (
      <label className="flex items-center gap-2 text-sm text-dim">
        on
        <select
          className="field"
          value={month}
          onChange={(e) => setMonth(Number(e.target.value))}
          data-testid="routine-month"
        >
          {MONTH_NAMES.map((m, i) => (
            <option key={m} value={i + 1}>
              {m}
            </option>
          ))}
        </select>
        <input
          className="field w-20"
          type="number"
          min={1}
          max={31}
          value={dayOfMonth}
          onChange={(e) => setDayOfMonth(e.target.value)}
          data-testid="routine-day-of-month"
        />
      </label>
    )}

    {kind === "Weekly" && weekdays.size === 0 && (
      <p className="text-xs text-red-ink">Pick at least one day.</p>
    )}
  </>
)}
```

with, at module level:

```tsx
const MONTH_NAMES = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];
```

(Check whether a `chip`/`chip-on` class exists elsewhere in the app — e.g. the composer's chips — and reuse it; otherwise use the existing `key` button class with an active variant.)

- [ ] **Step 8: Run the edit-page tests**

```bash
cd clients/web && bun run test:unit -- RoutineEditPage
```

Expected: pass.

- [ ] **Step 9: Typecheck, unit tests, build**

```bash
cd clients/web && bun run typecheck && bun run test:unit && bun run build
```

Expected: all clean.

- [ ] **Step 10: Commit**

```bash
git add clients/web/src
git commit -m "feat: calendar-style trigger controls in the routine form"
```

---

### Task 7: Docs — routines guide

**Files:**
- Modify: `docs/guide/routines.md`

- [ ] **Step 1: Update the guide**

1. In "What a routine is made of", the trigger list becomes:

```markdown
+ a trigger        (manually · repeatedly · once · daily · weekly · monthly · yearly)
```

2. In "Creating one → Trigger", after the *Once, at a time* bullet, add:

```markdown
     - *Daily / Weekly / Monthly / Yearly* — calendar triggers at a wall-clock
       time. Each carries its own IANA timezone (the form defaults to your
       browser's); weekly lets you pick any weekdays, monthly a day of the
       month, yearly a month and day.
```

3. In "How the schedule behaves", after the *Once* bullet, add:

```markdown
- **Calendar triggers** — daily, weekly, monthly and yearly schedules fire at
  their wall-clock time in the routine's own timezone. The next firing is the
  next occurrence after the previous one, so a server that was down while a
  firing came due runs **once, late**, never a backlog — the same contract as
  *Repeatedly*. A month without the day you picked (the 31st, 29–31 February)
  is skipped; Feb 29 recurs only in leap years. Around a daylight-saving
  change the wall-clock time is kept: a time that does not exist that day
  (spring forward) fires at the shifted time, and a time that occurs twice
  (fall back) fires once.
```

- [ ] **Step 2: Commit**

```bash
git add docs/guide/routines.md
git commit -m "docs: document calendar routine triggers"
```

---

### Task 8: Final verification and PR

**Files:** none (verification only)

- [ ] **Step 1: Format check (stable toolchain)**

```bash
cargo fmt --all -- --check
```

Expected: clean. If not, run `cargo fmt --all` (stable; never `+nightly`).

- [ ] **Step 2: Clippy**

```bash
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Expected: clean.

- [ ] **Step 3: Full workspace test suite**

```bash
cargo test --locked --workspace --all-features
```

Expected: all pass.

- [ ] **Step 4: Web unit tests, typecheck, build**

```bash
cd clients/web && bun run test:unit && bun run typecheck && bun run build
```

- [ ] **Step 5: Confirm no uncommitted generated drift**

```bash
git status --short
```

Expected: nothing (the TS regeneration from Task 1 is committed).

- [ ] **Step 6: Commit any stragglers, push, open the PR**

```bash
git add -A && git commit -m "chore: ..."  # only if something remains
git push -u origin feat/routine-recurrence
```

Open a PR titled `feat: calendar-style routine scheduling (daily/weekly/monthly/yearly)` with a concise body: what changed (wire union arms, JSON schedule column + migration 0026 backfill, jiff next-occurrence, form controls with browser-zone default), the three judgment calls (no "every N periods"; monthly skips short months; catch-up-once after downtime), and test coverage. Watch CI: fmt, clippy, cargo test (SQLite + PostgreSQL), web unit, web build, ts-types drift check. Fix any failure before reporting done.

---

## Self-Review Notes

- **Spec coverage:** wire model (Task 1), recurrence math + DST/skip semantics (Task 2), JSON storage + migration + strict read errors (Tasks 3–4), validation (Task 4), timer/catch-up semantics (Task 4 scheduler test, existing mechanics unchanged), CLI (Task 5), web form + browser-zone default + describeSchedule (Task 6), docs (Task 7), verification (Task 8). No spec section is left without a task.
- **Type consistency:** `next_occurrence(&RoutineSchedule, u64) -> Option<u64>` (Task 2) matches Task 4's `next_run_at` delegation; `RoutineRow.schedule: RoutineSchedule` (Task 4) matches Task 3's JSON column; TS `value.dayOfMonth`/`value.weekdays` camelCase matches the fluorite codegen and Task 6's payloads; `data-testid` names in Task 6 Step 5 tests match Step 7 markup (`routine-timezone-select`, `routine-time-input`, `weekday-mon`, `save-routine-button`).
- **Known jiff API facts (verified against jiff 0.2.28 source):** `TimeZone::get`, `Timestamp::from_millisecond`/`as_millisecond`/`to_zoned`, `Date::new`/`tomorrow`/`weekday`/`checked_add`, `Weekday::to_monday_one_offset`, `DateTime::new` (fallible), `DateTime::to_zoned` (Compatible default), `Zoned::date`/`timestamp`, `ToSpan::month`. All confirmed present.
