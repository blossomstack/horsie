# Routine recurrence: daily, weekly, monthly, yearly wall-clock triggers

Date: 2026-08-07 · Status: draft for review

## Context

A routine is an agent preset plus a fixed prompt and a trigger. Today the
trigger is one of:

- **Manual** — the run button and the API only.
- **Every** — recurring every `interval_secs` seconds (min 60), measured from
  the previous firing, so a server that was down resumes with one run rather
  than a backlog.
- **Once** — a single firing at an absolute instant.

People keep asking for calendar-style triggers: "triage the queue every morning
at 9", "summarise the week Friday 16:00", "first of the month report". This
adds the Google-Calendar-style recurrence kinds — daily, weekly (on chosen
weekdays), monthly (on a day of month), yearly (on a month and day) — each at a
wall-clock time in a per-routine timezone.

## Goals

- New trigger kinds: **Daily**, **Weekly** (pick any weekdays), **Monthly**
  (day-of-month), **Yearly** (month + day), each with `hour:minute` and an IANA
  timezone.
- Google-Calendar-style controls in the web form: timezone select defaulting to
  the browser's zone, weekday chips, day-of-month and month/day inputs.
- Existing semantics preserved: claim-before-run tick, run-history untouched by
  edits, timer unaffected by manual runs, advance-on-failure.

## Non-goals (explicitly out of scope)

- "Every N periods" intervals (every 2 weeks, every 3 months), "last Friday of
  the month", "every N days" from a date — any of these can be added later
  without changing the model.
- Generic cron-expression arm (a future power-user escape hatch; a text box,
  not calendar controls, and needs a cron parser).
- Per-schedule timezone on the existing **Once** arm — it stays an absolute
  instant, exactly as today.

## Wire model — `models/fluorite/routines.fl`

```fluorite
/// Day of the week, Mon first — the ordering the UI renders.
enum Weekday { Mon, Tue, Wed, Thu, Fri, Sat, Sun }

/// Every day at `hour:minute` in `timezone`.
struct DailySchedule { timezone: String, hour: u8, minute: u8 }

/// On the listed weekdays at `hour:minute` in `timezone`. At least one day;
/// duplicates are rejected at save.
struct WeeklySchedule { timezone: String, hour: u8, minute: u8, weekdays: Vec<Weekday> }

/// On `day_of_month` of every month in `timezone`. Months without that day
/// (the 31st, the 29th–31st in February) are skipped entirely.
struct MonthlySchedule { timezone: String, hour: u8, minute: u8, day_of_month: u8 }

/// On `month`/`day_of_month` every year in `timezone`. Invalid dates
/// (Feb 29 in a non-leap year) recur only when valid.
struct YearlySchedule { timezone: String, hour: u8, minute: u8, month: u8, day_of_month: u8 }

/// When a routine fires by itself. A union rather than a kind + optional
/// fields, so "every, with no interval" cannot be expressed.
#[type_tag = "type"]
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

Raw units (`hour`, `minute`, `day_of_month`, `month` as u8) match the existing
style (`interval_secs`, `at_ms`). Fluorite regenerates the Rust types at build
time and the TS types via the committed `generate-types` step; the new arms are
automatically available to both sides. The generated union is adjacently
tagged (`#[serde(tag = "type", content = "value")]`, payloads camelCase), so a
`Daily` schedule serializes as
`{"type":"Daily","value":{"timezone":"Asia/Shanghai","hour":9,"minute":0}}`.

## Next-occurrence semantics — new `server/src/routines/recurrence.rs`

`next_run_at` in `service.rs` keeps its signature and its meaning — "when
should this schedule next fire, given the moment it was armed" — but now
operates on the wire `RoutineSchedule` (see Storage) and delegates the four new
arms to `next_occurrence(schedule, now_ms)`, implemented with **jiff**
(already in `Cargo.lock` at 0.2.28, today as a transitive dependency; add it as
a direct server dependency with its bundled timezone database, so any IANA
zone resolves regardless of the host's own tz data).
`next_occurrence` walks calendar candidates in the routine's zone and returns
the first occurrence **strictly after `now`**:

- **Daily** — today at `hour:minute`; if that is ≤ now, tomorrow; and so on.
- **Weekly** — the next day in `weekdays` at `hour:minute` (wrapping to next
  week when the week's days are exhausted).
- **Monthly** — this month's `day_of_month`; months lacking that day are
  skipped to the next month (Jan 31 → Mar 31 in a non-leap year).
- **Yearly** — this year's `month`/`day_of_month`; an invalid date (Feb 29 in a
  non-leap year) skips to the next year.

**DST** — jiff's default `Compatible` disambiguation: a wall-clock time that
falls in a spring-forward gap fires at the shifted (post-gap) wall-clock time
that same day; a fall-back repeat fires once (the earlier offset). The calendar
never skips a day because of DST. Documented, deterministic, no option.

**Invalid timezone at compute time** — only reachable via corrupt storage
(save-time validation blocks it): `next_occurrence` returns `None` and the
routine idles rather than crashing the tick, same as a paused routine.

## Validation — `server/src/routines/service.rs`

At create/replace, all new arms validate: `jiff::tz::TimeZone::get(timezone)`
must succeed (IANA name; unknown zone → `RoutineError::Invalid`, HTTP 422);
`hour ≤ 23`; `minute ≤ 59`; `day_of_month ∈ 1..=31`; `month ∈ 1..=12`;
`weekdays` non-empty, no duplicates, in canonical Mon–Sun order. The existing
`Every` interval floor and the other checks are untouched.

## Storage — migration `0026` (both dialects, parity test requires it)

One JSON column replaces the three typed schedule columns. The column stores
the serialized wire `RoutineSchedule` verbatim, so the parallel storage
`Schedule` enum (`server/src/routines/store.rs`), the `from_columns` mapping,
and the `storage_schedule`/`wire_schedule` conversions in `service.rs` are
deleted — `RoutineRow.schedule` becomes the wire type.

`server/migrations/sqlite/0026_routine_schedule_json.sql`:

```sql
-- Routines: the trigger moves from three typed columns to one JSON column
-- holding the serialized `RoutineSchedule` wire union (adjacently tagged,
-- camelCase payloads). The backfill is exact string literals because we own
-- the wire shape; a NULL schedule on a legacy row is impossible because
-- every/once always carried their payload column. DROP COLUMN is fine here
-- (SQLite ≥ 3.35): none of the dropped columns is indexed or defaulted.
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

`server/migrations/postgres/0026_routine_schedule_json.sql`: the same, with the
three drops combined into one statement
(`ALTER TABLE routines DROP COLUMN schedule_kind, DROP COLUMN interval_secs,
DROP COLUMN at_ms;`). `enabled`/`next_run_at_ms` and the
`routines_next_run` index are untouched — the scheduler still works purely on
`next_run_at_ms`.

Store read path: `schedule` parses with `serde_json`; a row that does not parse
as a legal schedule is an error, never a silently-defaulted value — the same
strictness `from_columns` had.

## Timer behavior — unchanged mechanics, documented semantics

The 15-second tick, claim-before-run, and advance-on-failure all stay. Because
re-arming computes the next calendar occurrence **after now**, a routine whose
scheduled instant passed while the server was down fires **once, late** when
the server returns — the same "resume with one run, never a backlog" contract
the docs already promise for `Every`. A failed run likewise advances to the
next calendar occurrence. Manual runs and edits do not disturb the timer.

## CLI — `cli/src/routines.rs`

`schedule_label` gains arms and the detail view shows them:

- `daily 09:00 Asia/Shanghai`
- `weekly mon,wed,fri 09:00 Asia/Shanghai`
- `monthly 15th 09:00 Asia/Shanghai`
- `yearly feb-15 09:00 Asia/Shanghai`

(Weekday/month rendering from the new `Weekday` enum and the schedule payload;
the list table truncates long labels as it already does.)

## Web UI — `clients/web`

- **`RoutineEditPage.tsx`** — the trigger dropdown gains Daily / Weekly /
  Monthly / Yearly. The four new kinds share: a timezone `<select>` populated
  from `Intl.supportedValuesOf("timeZone")` (curated fallback for browsers
  without it) **defaulting to the browser's zone**
  (`Intl.DateTimeFormat().resolvedOptions().timeZone`), and an
  `<input type="time">`. Per kind: weekly shows seven weekday toggle chips with
  a "Mon–Fri" shortcut (≥ 1 required, save disabled otherwise); monthly shows a
  day-of-month number input (1–31); yearly shows a month `<select>` plus a day
  input. Client-side validation mirrors the server.
- **`lib/schedule.ts`** — `describeSchedule` renders e.g.
  `every Mon, Wed, Fri at 09:00 (Asia/Shanghai)`; shown on the list and detail
  pages. `formatInterval` is untouched.
- The generated `RoutineSchedule` TS type picks the new arms up automatically
  from fluorite.

## Docs

`docs/guide/routines.md` — new trigger kinds in "What a routine is made of" and
the trigger list; a new "Calendar triggers" subsection under "How the schedule
behaves" covering: the timezone is the routine's own (IANA name; the form
defaults to your browser's zone), DST gap/fold behavior, month-end and leap-day
skips, and catch-up-once after downtime.

## Testing

- **`recurrence.rs` unit tests** (pure, table-driven, no DB): daily today/
  tomorrow boundary; weekly same-day, wrap-around, multiple days; monthly
  day-31 skips and Feb; yearly leap-year; a DST spring-forward gap and a
  fall-back fold; a fixed instant verified in Asia/Shanghai vs UTC.
- **Service** — validation rejections for each new arm (bad zone, hour 24,
  day 0/32, month 13, empty weekdays, duplicate weekdays); `next_run_at` for
  each new arm.
- **Store** — all seven arms round-trip through the JSON column; a row whose
  `schedule` is invalid JSON errors on read; a migration test (the repo has the
  harness for it) that reconstructs the pre-0026 schema with legacy rows,
  applies 0026, and asserts the backfilled JSON and dropped columns.
- **Scheduler** — a weekly routine fires once when due and re-arms to the next
  occurrence (extend the existing pattern in `scheduler.rs` tests).
- **CLI** — `schedule_label`/detail rendering for the four new arms.
- **Web** — extend the routines form test: each new kind renders its controls,
  weekly requires a checked day, and save builds the right payload.
- E2E untouched (its fixture uses the default Manual schedule).

## Judgment calls (vetoable)

- **No "every N periods" interval** — kept out for scope; the union arms mean
  it can be added later as new payload fields without breaking clients.
- **Catch-up-once after downtime** — matches the existing `Every` contract and
  the docs' promise.
- **Monthly skips short months entirely** (Jan 31 → Mar 31) rather than rolling
  to month-end — the common convention for fixed-day-of-month schedules.
