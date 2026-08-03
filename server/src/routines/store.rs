//! SQLite storage for routines, sharing the config store's pool.
//!
//! The schedule is a sum type here and three columns in the table; the mapping
//! is the only place that knows both shapes. A row that cannot be read back as
//! a legal schedule is an error, never a silently-defaulted value.

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

const COLS: &str = "name, description, agent, prompt, schedule_kind, interval_secs, at_ms, \
                    enabled, next_run_at_ms, last_run_at_ms, last_session_id, last_error, \
                    created_at, updated_at";

/// When a routine fires by itself (storage twin of the wire
/// `routines::RoutineSchedule`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Only the run endpoint and the UI button.
    Manual,
    /// Every `interval_secs` seconds, measured from the previous firing.
    Every { interval_secs: u64 },
    /// Once, at `at_ms`; never re-armed.
    Once { at_ms: u64 },
}

impl Schedule {
    fn kind(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Every { .. } => "every",
            Self::Once { .. } => "once",
        }
    }

    fn interval_secs(&self) -> Option<i64> {
        match self {
            Self::Every { interval_secs } => Some(*interval_secs as i64),
            Self::Manual | Self::Once { .. } => None,
        }
    }

    fn at_ms(&self) -> Option<i64> {
        match self {
            Self::Once { at_ms } => Some(*at_ms as i64),
            Self::Manual | Self::Every { .. } => None,
        }
    }

    /// Rebuild from the three columns. The missing-payload cases are errors:
    /// defaulting an interval would silently change how often a routine runs.
    fn from_columns(
        kind: &str,
        interval_secs: Option<i64>,
        at_ms: Option<i64>,
    ) -> Result<Self, String> {
        match kind {
            "manual" => Ok(Self::Manual),
            "every" => interval_secs
                .map(|s| Self::Every {
                    interval_secs: s.max(0) as u64,
                })
                .ok_or_else(|| "routines.schedule: 'every' with no interval_secs".to_string()),
            "once" => at_ms
                .map(|m| Self::Once {
                    at_ms: m.max(0) as u64,
                })
                .ok_or_else(|| "routines.schedule: 'once' with no at_ms".to_string()),
            other => Err(format!("routines.schedule: unknown kind '{other}'")),
        }
    }
}

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
    pub prompt: String,
    pub schedule: Schedule,
    pub enabled: bool,
    pub next_run_at_ms: Option<u64>,
    pub last_run_at_ms: Option<u64>,
    pub last_session_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct RoutineStore {
    pool: SqlitePool,
}

impl RoutineStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list(&self) -> Result<Vec<RoutineRow>, String> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM routines ORDER BY name"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_routine).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<RoutineRow>, String> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM routines WHERE name = ?"))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_routine).transpose()
    }

    /// Enabled routines whose next run has come due. The scheduler's only read.
    pub async fn due(&self, now_ms: u64) -> Result<Vec<RoutineRow>, String> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM routines \
             WHERE enabled = 1 AND next_run_at_ms IS NOT NULL AND next_run_at_ms <= ? \
             ORDER BY next_run_at_ms"
        ))
        .bind(now_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_routine).collect()
    }

    /// Names of the routines configured to run a given agent preset.
    pub async fn using_agent(&self, agent: &str) -> Result<Vec<String>, String> {
        let rows = sqlx::query("SELECT name FROM routines WHERE agent = ? ORDER BY name")
            .bind(agent)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("name").map_err(|e| e.to_string()))
            .collect()
    }

    /// Insert; errs when the name is taken (no upsert — a silent overwrite
    /// would discard the existing routine).
    pub async fn insert(&self, row: &RoutineRow) -> Result<(), String> {
        sqlx::query(&format!(
            "INSERT INTO routines ({COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.agent)
        .bind(&row.prompt)
        .bind(row.schedule.kind())
        .bind(row.schedule.interval_secs())
        .bind(row.schedule.at_ms())
        .bind(i64::from(row.enabled))
        .bind(row.next_run_at_ms.map(|v| v as i64))
        .bind(row.last_run_at_ms.map(|v| v as i64))
        .bind(&row.last_session_id)
        .bind(&row.last_error)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create routine '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace of the definition. Returns false when no routine has that
    /// name. Run history (`last_*`) is deliberately untouched: editing a
    /// routine does not un-run it.
    pub async fn replace(&self, row: &RoutineRow) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE routines SET description = ?, agent = ?, prompt = ?, schedule_kind = ?, \
             interval_secs = ?, at_ms = ?, enabled = ?, next_run_at_ms = ?, updated_at = ? \
             WHERE name = ?",
        )
        .bind(&row.description)
        .bind(&row.agent)
        .bind(&row.prompt)
        .bind(row.schedule.kind())
        .bind(row.schedule.interval_secs())
        .bind(row.schedule.at_ms())
        .bind(i64::from(row.enabled))
        .bind(row.next_run_at_ms.map(|v| v as i64))
        .bind(&row.updated_at)
        .bind(&row.name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query("DELETE FROM routines WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    /// Record what a trigger did, and when the next one is due. One statement
    /// so the two outcome columns can never both be set.
    pub async fn record_run(
        &self,
        name: &str,
        at_ms: u64,
        outcome: &RunOutcome,
        next_run_at_ms: Option<u64>,
    ) -> Result<(), String> {
        let (session, error) = match outcome {
            RunOutcome::Started(id) => (Some(id.clone()), None),
            RunOutcome::Failed(msg) => (None, Some(msg.clone())),
        };
        sqlx::query(
            "UPDATE routines SET last_run_at_ms = ?, last_session_id = ?, last_error = ?, \
             next_run_at_ms = ? WHERE name = ?",
        )
        .bind(at_ms as i64)
        .bind(session)
        .bind(error)
        .bind(next_run_at_ms.map(|v| v as i64))
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn row_to_routine(row: &SqliteRow) -> Result<RoutineRow, String> {
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
        prompt: get("prompt")?,
        schedule: Schedule::from_columns(
            &get("schedule_kind")?,
            get_int("interval_secs")?,
            get_int("at_ms")?,
        )?,
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
    use std::str::FromStr;

    async fn store() -> (RoutineStore, SqlitePool, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (RoutineStore::new(pool.clone()), pool, tmp)
    }

    fn row(name: &str, schedule: Schedule) -> RoutineRow {
        RoutineRow {
            name: name.into(),
            description: "d".into(),
            agent: "reviewer".into(),
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

    #[tokio::test]
    async fn every_schedule_shape_round_trips() {
        let (s, _p, _t) = store().await;
        for (name, schedule) in [
            ("m", Schedule::Manual),
            ("e", Schedule::Every { interval_secs: 300 }),
            ("o", Schedule::Once { at_ms: 9_000 }),
        ] {
            s.insert(&row(name, schedule.clone())).await.unwrap();
            let got = s.get(name).await.unwrap().unwrap();
            assert_eq!(got, row(name, schedule));
        }
        assert_eq!(s.list().await.unwrap().len(), 3);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _p, _t) = store().await;
        s.insert(&row("a", Schedule::Manual)).await.unwrap();
        assert!(s.insert(&row("a", Schedule::Manual)).await.is_err());
    }

    #[tokio::test]
    async fn replace_swaps_the_definition_and_keeps_run_history() {
        let (s, _p, _t) = store().await;
        assert!(!s.replace(&row("ghost", Schedule::Manual)).await.unwrap());
        s.insert(&row("a", Schedule::Manual)).await.unwrap();
        s.record_run("a", 500, &RunOutcome::Started("sess-1".into()), None)
            .await
            .unwrap();

        let mut edited = row("a", Schedule::Every { interval_secs: 60 });
        edited.prompt = "new prompt".into();
        edited.updated_at = "2".into();
        edited.next_run_at_ms = Some(2_000);
        assert!(s.replace(&edited).await.unwrap());

        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.prompt, "new prompt");
        assert_eq!(got.schedule, Schedule::Every { interval_secs: 60 });
        assert_eq!(got.next_run_at_ms, Some(2_000));
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
        assert_eq!(got.last_session_id.as_deref(), Some("sess-1"));
        assert_eq!(got.last_run_at_ms, Some(500));
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _p, _t) = store().await;
        s.insert(&row("a", Schedule::Manual)).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn due_respects_the_timestamp_and_the_enabled_flag() {
        let (s, _p, _t) = store().await;
        let mut soon = row("soon", Schedule::Every { interval_secs: 60 });
        soon.next_run_at_ms = Some(1_000);
        let mut later = row("later", Schedule::Every { interval_secs: 60 });
        later.next_run_at_ms = Some(5_000);
        let mut paused = row("paused", Schedule::Every { interval_secs: 60 });
        paused.next_run_at_ms = Some(1_000);
        paused.enabled = false;
        let mut manual = row("manual", Schedule::Manual);
        manual.next_run_at_ms = None;
        for r in [&soon, &later, &paused, &manual] {
            s.insert(r).await.unwrap();
        }

        let names = |rows: Vec<RoutineRow>| rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert!(names(s.due(999).await.unwrap()).is_empty());
        assert_eq!(names(s.due(1_000).await.unwrap()), vec!["soon"]);
        assert_eq!(names(s.due(9_999).await.unwrap()), vec!["soon", "later"]);
    }

    #[tokio::test]
    async fn record_run_replaces_the_previous_outcome() {
        let (s, _p, _t) = store().await;
        s.insert(&row("a", Schedule::Every { interval_secs: 60 }))
            .await
            .unwrap();

        s.record_run(
            "a",
            100,
            &RunOutcome::Failed("vendor offline".into()),
            Some(160),
        )
        .await
        .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.last_error.as_deref(), Some("vendor offline"));
        assert_eq!(got.last_session_id, None);
        assert_eq!(got.next_run_at_ms, Some(160));

        // A later success must not leave the old error behind.
        s.record_run("a", 200, &RunOutcome::Started("sess-9".into()), Some(260))
            .await
            .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.last_session_id.as_deref(), Some("sess-9"));
        assert_eq!(got.last_error, None);
        assert_eq!(got.last_run_at_ms, Some(200));
    }

    #[tokio::test]
    async fn using_agent_finds_every_referencing_routine() {
        let (s, _p, _t) = store().await;
        let mut other = row("b", Schedule::Manual);
        other.agent = "fixer".into();
        s.insert(&row("a", Schedule::Manual)).await.unwrap();
        s.insert(&other).await.unwrap();
        assert_eq!(s.using_agent("reviewer").await.unwrap(), vec!["a"]);
        assert!(s.using_agent("ghost").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_schedule_row_missing_its_payload_is_an_error() {
        // Not a defaulted interval: a routine silently running at some other
        // cadence than the one it was saved with is worse than a load failure.
        let (s, pool, _t) = store().await;
        s.insert(&row("a", Schedule::Every { interval_secs: 60 }))
            .await
            .unwrap();
        sqlx::query("UPDATE routines SET interval_secs = NULL WHERE name = 'a'")
            .execute(&pool)
            .await
            .unwrap();
        let err = s.get("a").await.unwrap_err();
        assert!(err.contains("interval_secs"), "{err}");
    }
}
