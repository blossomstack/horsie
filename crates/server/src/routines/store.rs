//! Storage for routines, sharing the config store's database.
//!
//! The schedule and the environment are stored verbatim as their serialized
//! wire unions, one JSON column each. A row whose JSON cannot be read back as a
//! legal value is an error, never a silently-defaulted one.

use crate::auth::UserId;
use crate::db::Db;
use horsie_models::environments::EnvironmentSpec;
use horsie_models::routines::RoutineSchedule;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, description, agent, environment, prompt, schedule, enabled, next_run_at_ms, \
                    last_run_at_ms, last_session_id, last_error, created_at, updated_at";

/// What one trigger did. One value rather than two nullable fields, so a run
/// can never record both a session and an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// A session was created; the run itself now reports through that session.
    Started(String),
    /// No session was created — a missing agent, an offline vendor.
    Failed(String),
}

/// One row of the `routines` table.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutineRow {
    pub name: String,
    pub description: String,
    pub agent: String,
    pub environment: EnvironmentSpec,
    pub prompt: String,
    pub schedule: RoutineSchedule,
    pub enabled: bool,
    pub next_run_at_ms: Option<u64>,
    pub last_run_at_ms: Option<u64>,
    pub last_session_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct RoutineStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: UserId,
}

impl RoutineStore {
    pub fn new(db: Db, user: UserId) -> Self {
        Self { db, user }
    }

    pub async fn list(&self) -> Result<Vec<RoutineRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM routines WHERE user_id = ? ORDER BY name"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_routine).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<RoutineRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM routines WHERE user_id = ? AND name = ?"
        )))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_routine).transpose()
    }

    /// Enabled routines whose next run has come due, across every account,
    /// each paired with the account that owns it.
    ///
    /// Deliberately NOT scoped, and an associated function rather than a
    /// method because it belongs to no one account: the scheduler is one timer
    /// for the deployment. Each routine is then armed and run *as its owner*.
    /// On the scope audit's allowlist for exactly this reason.
    ///
    /// The only "what is due" read there is. A scoped twin used to exist beside
    /// it, and a second answer to that question is a second thing to keep
    /// correct — the scoped one silently under-reports the moment a deployment
    /// has two accounts.
    pub async fn due_across_all_users(
        db: &Db,
        now_ms: u64,
    ) -> Result<Vec<(UserId, RoutineRow)>, String> {
        let rows = sqlx::query(&db.q(&format!(
            "SELECT user_id, {COLS} FROM routines \
             WHERE enabled = 1 AND next_run_at_ms IS NOT NULL AND next_run_at_ms <= ? \
             ORDER BY next_run_at_ms"
        )))
        .bind(now_ms as i64)
        .fetch_all(db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| {
                let owner = r
                    .try_get::<String, _>("user_id")
                    .map_err(|e| e.to_string())?;
                Ok((UserId::new(owner), row_to_routine(r)?))
            })
            .collect()
    }

    /// Names of the routines configured to run a given agent preset.
    pub async fn using_agent(&self, agent: &str) -> Result<Vec<String>, String> {
        let rows = sqlx::query(
            &self
                .db
                .q("SELECT name FROM routines WHERE user_id = ? AND agent = ? ORDER BY name"),
        )
        .bind(self.user.as_str())
        .bind(agent)
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("name").map_err(|e| e.to_string()))
            .collect()
    }

    /// Insert; errs when the name is taken (no upsert — a silent overwrite
    /// would discard the existing routine).
    pub async fn insert(&self, row: &RoutineRow) -> Result<(), String> {
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO routines (user_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.user.as_str())
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.agent)
        .bind(serde_json::to_string(&row.environment).map_err(|e| e.to_string())?)
        .bind(&row.prompt)
        .bind(serde_json::to_string(&row.schedule).map_err(|e| e.to_string())?)
        .bind(i64::from(row.enabled))
        .bind(row.next_run_at_ms.map(|v| v as i64))
        .bind(row.last_run_at_ms.map(|v| v as i64))
        .bind(&row.last_session_id)
        .bind(&row.last_error)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create routine '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace of the definition. Returns false when no routine has that
    /// name. Run history (`last_*`) is deliberately untouched: editing a
    /// routine does not un-run it.
    pub async fn replace(&self, row: &RoutineRow) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(
            "UPDATE routines SET description = ?, agent = ?, environment = ?, prompt = ?, schedule = ?, \
             enabled = ?, next_run_at_ms = ?, updated_at = ? \
             WHERE user_id = ? AND name = ?",
        ))
        .bind(&row.description)
        .bind(&row.agent)
        .bind(serde_json::to_string(&row.environment).map_err(|e| e.to_string())?)
        .bind(&row.prompt)
        .bind(serde_json::to_string(&row.schedule).map_err(|e| e.to_string())?)
        .bind(i64::from(row.enabled))
        .bind(row.next_run_at_ms.map(|v| v as i64))
        .bind(&row.updated_at)
        .bind(self.user.as_str())
        .bind(&row.name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM routines WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    /// Move the timer to its next firing. The scheduler calls this *before*
    /// starting a run, so a run that outlives a tick cannot be picked up as
    /// still-due and started a second time.
    pub async fn arm(&self, name: &str, next_run_at_ms: Option<u64>) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("UPDATE routines SET next_run_at_ms = ? WHERE user_id = ? AND name = ?"),
        )
        .bind(next_run_at_ms.map(|v| v as i64))
        .bind(self.user.as_str())
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record what a trigger did. One statement so the two outcome columns can
    /// never both be set, and deliberately not the timer: arming is the
    /// scheduler's business and a manual run must not disturb it.
    pub async fn record_run(
        &self,
        name: &str,
        at_ms: u64,
        outcome: &RunOutcome,
    ) -> Result<(), String> {
        let (session, error) = match outcome {
            RunOutcome::Started(id) => (Some(id.clone()), None),
            RunOutcome::Failed(msg) => (None, Some(msg.clone())),
        };
        sqlx::query(&self.db.q(
            "UPDATE routines SET last_run_at_ms = ?, last_session_id = ?, last_error = ? \
             WHERE user_id = ? AND name = ?",
        ))
        .bind(at_ms as i64)
        .bind(session)
        .bind(error)
        .bind(self.user.as_str())
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn row_to_routine(row: &AnyRow) -> Result<RoutineRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_opt = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    let get_int = |c: &str| row.try_get::<Option<i64>, _>(c).map_err(|e| e.to_string());
    Ok(RoutineRow {
        name: get("name")?,
        description: get("description")?,
        agent: get("agent")?,
        environment: serde_json::from_str(&get("environment")?)
            .map_err(|e| format!("routines.environment: {e}"))?,
        prompt: get("prompt")?,
        schedule: serde_json::from_str(&get("schedule")?)
            .map_err(|e| format!("routines.schedule: {e}"))?,
        enabled: get_int("enabled")?.unwrap_or(0) != 0,
        next_run_at_ms: get_int("next_run_at_ms")?.map(|v| v.max(0) as u64),
        last_run_at_ms: get_int("last_run_at_ms")?.map(|v| v.max(0) as u64),
        last_session_id: get_opt("last_session_id")?,
        last_error: get_opt("last_error")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::environments::{NamedEnvironment, RuntimeEnvironment};
    use horsie_models::routines::{
        DailySchedule, EverySchedule, ManualSchedule, MonthlySchedule, OnceSchedule, Weekday,
        WeeklySchedule, YearlySchedule,
    };
    use horsie_models::session_api::RepoConfig;
    use std::collections::HashMap;

    async fn store() -> (RoutineStore, Db) {
        let db = crate::db::testing::db().await;
        (
            RoutineStore::new(db.clone(), crate::auth::UserId::new("1")),
            db,
        )
    }

    fn row(name: &str, schedule: RoutineSchedule) -> RoutineRow {
        RoutineRow {
            name: name.into(),
            description: "d".into(),
            agent: "reviewer".into(),
            environment: EnvironmentSpec::Runtime(RuntimeEnvironment {
                vendor: "local".into(),
                repos: None,
            }),
            prompt: "check the queue".into(),
            schedule,
            enabled: true,
            next_run_at_ms: Some(1_000),
            last_run_at_ms: None,
            last_session_id: None,
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    fn all_schedules() -> [(&'static str, RoutineSchedule); 7] {
        [
            ("m", RoutineSchedule::Manual(ManualSchedule {})),
            (
                "e",
                RoutineSchedule::Every(EverySchedule { interval_secs: 300 }),
            ),
            ("o", RoutineSchedule::Once(OnceSchedule { at_ms: 9_000 })),
            (
                "d",
                RoutineSchedule::Daily(DailySchedule {
                    timezone: "Asia/Shanghai".into(),
                    hour: 9,
                    minute: 30,
                }),
            ),
            (
                "w",
                RoutineSchedule::Weekly(WeeklySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                    weekdays: vec![Weekday::Mon, Weekday::Fri],
                }),
            ),
            (
                "mo",
                RoutineSchedule::Monthly(MonthlySchedule {
                    timezone: "UTC".into(),
                    hour: 0,
                    minute: 0,
                    day_of_month: 31,
                }),
            ),
            (
                "y",
                RoutineSchedule::Yearly(YearlySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                    month: 2,
                    day_of_month: 29,
                }),
            ),
        ]
    }

    #[tokio::test]
    async fn every_schedule_shape_round_trips() {
        let (s, _db) = store().await;
        for (name, schedule) in all_schedules() {
            s.insert(&row(name, schedule.clone())).await.unwrap();
            let got = s.get(name).await.unwrap().unwrap();
            assert_eq!(got, row(name, schedule));
        }
        assert_eq!(s.list().await.unwrap().len(), 7);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_environment_shape_round_trips() {
        let (s, _db) = store().await;
        for (name, environment) in [
            (
                "adhoc",
                EnvironmentSpec::Runtime(RuntimeEnvironment {
                    vendor: "fly".into(),
                    repos: Some(vec![RepoConfig {
                        url: "https://github.com/o/api".into(),
                        git_ref: Some("dev".into()),
                        dir: None,
                    }]),
                }),
            ),
            (
                "named",
                EnvironmentSpec::Named(NamedEnvironment {
                    name: "staging".into(),
                }),
            ),
        ] {
            let mut r = row(name, RoutineSchedule::Manual(ManualSchedule {}));
            r.environment = environment.clone();
            s.insert(&r).await.unwrap();
            assert_eq!(s.get(name).await.unwrap().unwrap().environment, environment);
        }
    }

    #[tokio::test]
    async fn a_replace_swaps_the_environment() {
        let (s, _db) = store().await;
        let mut r = row("nightly", RoutineSchedule::Manual(ManualSchedule {}));
        s.insert(&r).await.unwrap();
        r.environment = EnvironmentSpec::Named(NamedEnvironment {
            name: "staging".into(),
        });
        assert!(s.replace(&r).await.unwrap());
        assert_eq!(
            s.get("nightly").await.unwrap().unwrap().environment,
            r.environment
        );
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _db) = store().await;
        s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
            .await
            .unwrap();
        assert!(
            s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replace_swaps_the_definition_and_keeps_run_history() {
        let (s, _db) = store().await;
        assert!(
            !s.replace(&row("ghost", RoutineSchedule::Manual(ManualSchedule {})))
                .await
                .unwrap()
        );
        s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
            .await
            .unwrap();
        s.record_run("a", 500, &RunOutcome::Started("sess-1".into()))
            .await
            .unwrap();

        let mut edited = row(
            "a",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        );
        edited.prompt = "new prompt".into();
        edited.updated_at = "2".into();
        edited.next_run_at_ms = Some(2_000);
        assert!(s.replace(&edited).await.unwrap());

        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.prompt, "new prompt");
        assert_eq!(
            got.schedule,
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 })
        );
        assert_eq!(got.next_run_at_ms, Some(2_000));
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
        assert_eq!(got.last_session_id.as_deref(), Some("sess-1"));
        assert_eq!(got.last_run_at_ms, Some(500));
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _db) = store().await;
        s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
            .await
            .unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn due_respects_the_timestamp_and_the_enabled_flag() {
        let (s, db) = store().await;
        let mut soon = row(
            "soon",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        );
        soon.next_run_at_ms = Some(1_000);
        let mut later = row(
            "later",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        );
        later.next_run_at_ms = Some(5_000);
        let mut paused = row(
            "paused",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        );
        paused.next_run_at_ms = Some(1_000);
        paused.enabled = false;
        let mut manual = row("manual", RoutineSchedule::Manual(ManualSchedule {}));
        manual.next_run_at_ms = None;
        for r in [&soon, &later, &paused, &manual] {
            s.insert(r).await.unwrap();
        }

        let names = |rows: Vec<(crate::auth::UserId, RoutineRow)>| {
            rows.into_iter().map(|(_, r)| r.name).collect::<Vec<_>>()
        };
        let due = async |at| RoutineStore::due_across_all_users(&db, at).await.unwrap();
        assert!(names(due(999).await).is_empty());
        assert_eq!(names(due(1_000).await), vec!["soon"]);
        assert_eq!(names(due(9_999).await), vec!["soon", "later"]);
    }

    #[tokio::test]
    async fn record_run_replaces_the_previous_outcome_and_leaves_the_timer_alone() {
        let (s, _db) = store().await;
        s.insert(&row(
            "a",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        ))
        .await
        .unwrap();

        s.record_run("a", 100, &RunOutcome::Failed("vendor offline".into()))
            .await
            .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.last_error.as_deref(), Some("vendor offline"));
        assert_eq!(got.last_session_id, None);
        assert_eq!(
            got.next_run_at_ms,
            Some(1_000),
            "recording a run must not move the timer"
        );

        s.record_run("a", 200, &RunOutcome::Started("sess-9".into()))
            .await
            .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.last_session_id.as_deref(), Some("sess-9"));
        assert_eq!(got.last_error, None);
        assert_eq!(got.last_run_at_ms, Some(200));
    }

    #[tokio::test]
    async fn arm_moves_the_timer_and_disarming_takes_it_out_of_due() {
        let (s, db) = store().await;
        s.insert(&row(
            "a",
            RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
        ))
        .await
        .unwrap();
        s.arm("a", Some(61_000)).await.unwrap();
        assert_eq!(
            s.get("a").await.unwrap().unwrap().next_run_at_ms,
            Some(61_000)
        );
        assert!(
            RoutineStore::due_across_all_users(&db, 60_999)
                .await
                .unwrap()
                .is_empty()
        );

        s.arm("a", None).await.unwrap();
        assert_eq!(s.get("a").await.unwrap().unwrap().next_run_at_ms, None);
        assert!(
            RoutineStore::due_across_all_users(&db, u64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn using_agent_finds_every_referencing_routine() {
        let (s, _db) = store().await;
        let mut other = row("b", RoutineSchedule::Manual(ManualSchedule {}));
        other.agent = "fixer".into();
        s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
            .await
            .unwrap();
        s.insert(&other).await.unwrap();
        assert_eq!(s.using_agent("reviewer").await.unwrap(), vec!["a"]);
        assert!(s.using_agent("ghost").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_schedule_row_with_invalid_json_is_an_error() {
        // Not a silently-defaulted schedule: a routine running at some other
        // cadence than the one it was saved with is worse than a load failure.
        let (s, db) = store().await;
        s.insert(&row("a", RoutineSchedule::Manual(ManualSchedule {})))
            .await
            .unwrap();
        sqlx::query("UPDATE routines SET schedule = '{broken' WHERE name = 'a'")
            .execute(db.pool())
            .await
            .unwrap();
        let err = s.get("a").await.unwrap_err();
        assert!(err.contains("schedule"), "{err}");
    }

    /// SQLite-only, like the 0006 test: it builds the pre-0026 schema by hand
    /// and then applies exactly 0026, pinning the backfill to the wire JSON.
    #[tokio::test]
    async fn migration_0026_backfills_schedule_json_and_drops_the_typed_columns() {
        let pool = &crate::db::testing::unmigrated_sqlite().await;

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
            assert!(
                !cols.iter().any(|c| c == dropped),
                "{dropped} still present"
            );
        }
    }
}
