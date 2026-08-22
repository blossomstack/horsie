//! Storage for the agent-run index, sharing the config store's database.
//!
//! Two writes per run and no more. See the 0043 migration for why the table is
//! this narrow and why that matters.

use crate::db::Db;
use crate::projects::ProjectId;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "session_id, agent_id, preset, status, started_at, ended_at";

/// One agent run, as the index holds it.
///
/// Deliberately not a projection of `AgentEntry`: that is the roster's shape and
/// carries a dozen things this table has no column for. Keeping them apart is
/// what stops the roster growing a field and this table quietly needing a
/// migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRunRow {
    pub session_id: String,
    /// `"main"`, or the agent's uuid.
    pub agent_id: String,
    /// The preset this agent's settings were flattened from; `None` when they
    /// were supplied inline.
    pub preset: Option<String>,
    pub status: String,
    pub started_at: i64,
    /// `None` while the run is still going.
    pub ended_at: Option<i64>,
}

impl AgentRunRow {
    /// Whether this run is over.
    ///
    /// The status, not `ended_at`: a main agent has no end stamp — it is as old
    /// as its session and never *completes* — so reading the timestamp would
    /// call every idle conversation unfinished.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// What to narrow a listing by. Every field is a conjunction; all absent lists
/// every run this project holds, newest first.
#[derive(Clone, Debug, Default)]
pub struct AgentRunFilter {
    /// Runs of this preset. The query the index exists for.
    pub preset: Option<String>,
    /// Runs within this one session.
    pub session_id: Option<String>,
    /// Runs that reached this status.
    pub status: Option<String>,
    /// Runs that started at or after this epoch-ms stamp.
    pub since_ms: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

pub struct AgentRunStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    project: ProjectId,
}

impl AgentRunStore {
    pub fn new(db: Db, project: ProjectId) -> Self {
        Self { db, project }
    }

    /// Record runs that have just appeared, and close ones that have just
    /// ended. One transaction, so a reader never sees half a batch.
    ///
    /// Idempotent in both directions: an upsert that re-states what is already
    /// there is a no-op, which is what lets the caller re-send its whole roster
    /// after a crash without checking first.
    ///
    /// `started_at` is written once and never overwritten. It is the one fact
    /// here that a later observation could get *wrong* — a main agent reports
    /// zero — and re-stamping it on every touch would make "oldest run" mean
    /// "least recently written".
    pub async fn record(&self, rows: &[AgentRunRow]) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        for row in rows {
            sqlx::query(&self.db.q(&format!(
                "INSERT INTO agent_runs (project_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (project_id, session_id, agent_id) DO UPDATE SET \
                 preset = excluded.preset, status = excluded.status, \
                 ended_at = excluded.ended_at"
            )))
            .bind(self.project.as_str())
            .bind(&row.session_id)
            .bind(&row.agent_id)
            .bind(&row.preset)
            .bind(&row.status)
            .bind(row.started_at)
            .bind(row.ended_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("record agent run {}/{}: {e}", row.session_id, row.agent_id))?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Replace everything this session has in the index with `rows`.
    ///
    /// The load-time repair, and the only writer that deletes. A crash between
    /// an agent appearing and its row landing leaves the index short; a session
    /// deleting an agent leaves it long. Both are healed the next time the
    /// session is loaded, because the actor's own state is the source of truth
    /// and this makes the table agree with it.
    pub async fn reconcile(&self, session_id: &str, rows: &[AgentRunRow]) -> Result<(), String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(
            &self
                .db
                .q("DELETE FROM agent_runs WHERE project_id = ? AND session_id = ?"),
        )
        .bind(self.project.as_str())
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        for row in rows {
            sqlx::query(&self.db.q(&format!(
                "INSERT INTO agent_runs (project_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?)"
            )))
            .bind(self.project.as_str())
            .bind(&row.session_id)
            .bind(&row.agent_id)
            .bind(&row.preset)
            .bind(&row.status)
            .bind(row.started_at)
            .bind(row.ended_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Drop a whole session's runs. Called when the session itself is deleted —
    /// an index pointing at transcripts that no longer exist is worse than no
    /// index, because every hit is a dead end.
    pub async fn forget_session(&self, session_id: &str) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("DELETE FROM agent_runs WHERE project_id = ? AND session_id = ?"),
        )
        .bind(self.project.as_str())
        .bind(session_id)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Matching runs, newest first.
    ///
    /// Ordered by `started_at` and then by the primary key, because a batch of
    /// runs recorded in one persist shares a millisecond: without the tiebreak
    /// two pages of one listing can interleave and a reader paging through
    /// misses rows it never saw.
    pub async fn list(&self, filter: &AgentRunFilter) -> Result<Vec<AgentRunRow>, String> {
        let mut sql = format!("SELECT {COLS} FROM agent_runs WHERE project_id = ?");
        if filter.preset.is_some() {
            sql.push_str(" AND preset = ?");
        }
        if filter.session_id.is_some() {
            sql.push_str(" AND session_id = ?");
        }
        if filter.status.is_some() {
            sql.push_str(" AND status = ?");
        }
        if filter.since_ms.is_some() {
            sql.push_str(" AND started_at >= ?");
        }
        sql.push_str(" ORDER BY started_at DESC, session_id, agent_id LIMIT ? OFFSET ?");

        let mut query = sqlx::query(&self.db.q(&sql)).bind(self.project.as_str());
        if let Some(p) = &filter.preset {
            query = query.bind(p);
        }
        if let Some(s) = &filter.session_id {
            query = query.bind(s);
        }
        if let Some(s) = &filter.status {
            query = query.bind(s);
        }
        if let Some(t) = filter.since_ms {
            query = query.bind(t);
        }
        let rows = query
            .bind(i64::try_from(filter.limit).unwrap_or(i64::MAX))
            .bind(i64::try_from(filter.offset).unwrap_or(0))
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_run).collect()
    }
}

fn row_to_run(row: &AnyRow) -> Result<AgentRunRow, String> {
    Ok(AgentRunRow {
        session_id: row.try_get("session_id").map_err(|e| e.to_string())?,
        agent_id: row.try_get("agent_id").map_err(|e| e.to_string())?,
        preset: row.try_get("preset").map_err(|e| e.to_string())?,
        status: row.try_get("status").map_err(|e| e.to_string())?,
        started_at: row.try_get("started_at").map_err(|e| e.to_string())?,
        ended_at: row.try_get("ended_at").map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> AgentRunStore {
        AgentRunStore::new(
            crate::db::testing::db().await,
            crate::projects::ProjectId::new("1"),
        )
    }

    fn run(session: &str, agent: &str, preset: Option<&str>, started: i64) -> AgentRunRow {
        AgentRunRow {
            session_id: session.into(),
            agent_id: agent.into(),
            preset: preset.map(str::to_string),
            status: "running".into(),
            started_at: started,
            ended_at: None,
        }
    }

    #[tokio::test]
    async fn a_run_round_trips_every_column() {
        let s = store().await;
        let mut r = run("s1", "main", Some("reviewer"), 1_000);
        r.status = "completed".into();
        r.ended_at = Some(2_000);
        s.record(std::slice::from_ref(&r)).await.unwrap();
        let got = s
            .list(&AgentRunFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got, vec![r]);
    }

    /// The query the table exists for.
    #[tokio::test]
    async fn listing_by_preset_returns_only_that_presets_runs() {
        let s = store().await;
        s.record(&[
            run("s1", "main", Some("reviewer"), 1),
            run("s2", "main", Some("deployer"), 2),
            run("s3", "main", None, 3),
        ])
        .await
        .unwrap();
        let got = s
            .list(&AgentRunFilter {
                preset: Some("reviewer".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, "s1");
    }

    /// `NULL`, not `""`. An ad-hoc run must not answer a search for a preset —
    /// including one that happens to be named the empty string.
    #[tokio::test]
    async fn an_ad_hoc_run_matches_no_preset() {
        let s = store().await;
        s.record(&[run("s1", "main", None, 1)]).await.unwrap();
        for name in ["", "reviewer"] {
            let got = s
                .list(&AgentRunFilter {
                    preset: Some(name.into()),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(
                got.is_empty(),
                "an ad-hoc run answered a search for '{name}'"
            );
        }
    }

    #[tokio::test]
    async fn runs_come_back_newest_first_and_page() {
        let s = store().await;
        s.record(&[
            run("s1", "main", None, 100),
            run("s2", "main", None, 300),
            run("s3", "main", None, 200),
        ])
        .await
        .unwrap();
        let page = |limit, offset| {
            let s = &s;
            async move {
                s.list(&AgentRunFilter {
                    limit,
                    offset,
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.session_id)
                .collect::<Vec<_>>()
            }
        };
        assert_eq!(page(10, 0).await, vec!["s2", "s3", "s1"]);
        assert_eq!(page(2, 0).await, vec!["s2", "s3"]);
        assert_eq!(page(2, 2).await, vec!["s1"]);
    }

    /// Runs recorded in one persist share a millisecond. Without the primary-key
    /// tiebreak their order is whatever the planner feels like, and a reader
    /// paging through can see one row twice and another never.
    #[tokio::test]
    async fn runs_at_the_same_instant_have_a_stable_order() {
        let s = store().await;
        s.record(&[
            run("s1", "c", None, 500),
            run("s1", "a", None, 500),
            run("s1", "b", None, 500),
        ])
        .await
        .unwrap();
        let ids = |rows: Vec<AgentRunRow>| rows.into_iter().map(|r| r.agent_id).collect::<Vec<_>>();
        let first = ids(s
            .list(&AgentRunFilter {
                limit: 2,
                ..Default::default()
            })
            .await
            .unwrap());
        let second = ids(s
            .list(&AgentRunFilter {
                limit: 2,
                offset: 2,
                ..Default::default()
            })
            .await
            .unwrap());
        assert_eq!(first, vec!["a", "b"]);
        assert_eq!(second, vec!["c"]);
    }

    /// The second of the two writes a run ever gets.
    #[tokio::test]
    async fn recording_a_run_again_closes_it_without_moving_its_start() {
        let s = store().await;
        s.record(&[run("s1", "sub", Some("reviewer"), 1_000)])
            .await
            .unwrap();
        let mut ended = run("s1", "sub", Some("reviewer"), 9_999);
        ended.status = "failed".into();
        ended.ended_at = Some(5_000);
        s.record(&[ended]).await.unwrap();

        let got = s
            .list(&AgentRunFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            got.len(),
            1,
            "the second write updates rather than duplicates"
        );
        assert_eq!(got[0].status, "failed");
        assert_eq!(got[0].ended_at, Some(5_000));
        assert_eq!(
            got[0].started_at, 1_000,
            "a later observation must not restamp when the run began"
        );
    }

    #[tokio::test]
    async fn reconcile_makes_the_index_agree_with_the_session() {
        let s = store().await;
        s.record(&[
            run("s1", "main", None, 1),
            run("s1", "gone", None, 2),
            run("s2", "main", None, 3),
        ])
        .await
        .unwrap();
        // s1 now hosts a different set: one survivor and one the index missed.
        s.reconcile(
            "s1",
            &[run("s1", "main", None, 1), run("s1", "new", None, 4)],
        )
        .await
        .unwrap();

        let mut got: Vec<String> = s
            .list(&AgentRunFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|r| format!("{}/{}", r.session_id, r.agent_id))
            .collect();
        got.sort();
        assert_eq!(got, vec!["s1/main", "s1/new", "s2/main"]);
    }

    /// A hit pointing at a deleted session is a dead end, and a tuning agent
    /// reading the index would spend a call per stale row to find that out.
    #[tokio::test]
    async fn forgetting_a_session_drops_only_its_runs() {
        let s = store().await;
        s.record(&[run("s1", "main", None, 1), run("s2", "main", None, 2)])
            .await
            .unwrap();
        s.forget_session("s1").await.unwrap();
        let got = s
            .list(&AgentRunFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, "s2");
    }

    #[tokio::test]
    async fn filters_narrow_together() {
        let s = store().await;
        let mut done = run("s1", "a", Some("reviewer"), 500);
        done.status = "completed".into();
        done.ended_at = Some(600);
        s.record(&[
            done,
            run("s1", "b", Some("reviewer"), 500),
            run("s2", "a", Some("reviewer"), 100),
        ])
        .await
        .unwrap();

        let got = s
            .list(&AgentRunFilter {
                preset: Some("reviewer".into()),
                session_id: Some("s1".into()),
                status: Some("completed".into()),
                since_ms: Some(200),
                limit: 10,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].agent_id, "a");
    }

    #[tokio::test]
    async fn recording_nothing_is_not_an_error() {
        let s = store().await;
        s.record(&[]).await.unwrap();
        assert!(
            s.list(&AgentRunFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn a_main_agent_is_terminal_only_by_status() {
        // A main agent has no end stamp, so `ended_at` cannot decide this.
        let idle = run("s1", "main", None, 0);
        assert!(!idle.is_terminal());
        let mut done = idle.clone();
        done.status = "completed".into();
        assert!(done.is_terminal());
    }
}
