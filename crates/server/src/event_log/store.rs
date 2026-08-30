//! The event log's storage, and the trait a deployment swaps.
//!
//! One implementation today, backed by the same database everything else uses.
//! The trait exists because Kafka and Redis Streams are the stated destination,
//! not because there is a second implementation to abstract over now — so it
//! holds the three operations a log has and nothing else. Anything a particular
//! backend can do better belongs inside that backend.

use super::ProjectEvent;
use crate::db::{Db, Dialect};
use crate::projects::ProjectId;
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    #[error("event log unavailable: {0}")]
    Backend(String),
    /// An event could not be encoded. A bug in the payload type rather than a
    /// fault of the deployment, and kept apart for that reason: retrying it
    /// would fail identically for ever.
    #[error("could not encode an event: {0}")]
    Encode(String),
}

fn backend(e: impl std::fmt::Display) -> EventLogError {
    EventLogError::Backend(e.to_string())
}

/// One event as a consumer receives it.
///
/// `id` is the log's own position and is what an ack names. It is deliberately
/// not [`ProjectEvent::id`]: that one dedups *production*, this one orders
/// *delivery*, and conflating them would mean a consumer's cursor moved
/// whenever a producer re-derived an old event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub id: i64,
    pub event: ProjectEvent,
}

/// A durable, ordered, at-least-once log.
///
/// Pull rather than push, which is what a durable log actually is underneath —
/// Kafka's consumer API is a poll too. A push-shaped trait would have to invent
/// a delivery loop that every backend then fights.
#[async_trait::async_trait]
pub trait EventLog: Send + Sync {
    /// Append events, ignoring any already present under the same id.
    async fn append(&self, stream: &str, events: &[ProjectEvent]) -> Result<(), EventLogError>;

    /// The next events for `group`, oldest first, starting after its cursor.
    ///
    /// Returns the same events again on the next call until they are acked.
    /// That is the at-least-once guarantee, and it is why every consumer has to
    /// be idempotent in its own right.
    async fn read(
        &self,
        stream: &str,
        group: &str,
        limit: i64,
    ) -> Result<Vec<Delivered>, EventLogError>;

    /// Move `group`'s cursor to `up_to`, which must be an id it was handed.
    async fn ack(&self, stream: &str, group: &str, up_to: i64) -> Result<(), EventLogError>;
}

/// The log, in this deployment's database.
pub struct DbEventLog {
    db: Db,
    project: ProjectId,
}

impl DbEventLog {
    #[must_use]
    pub fn new(db: Db, project: ProjectId) -> Self {
        Self { db, project }
    }

    /// Drop events every one of `groups` has acked.
    ///
    /// **The group list is required and there is no age-based fallback.** A
    /// floor that trimmed on time alone would be safe for a status event, which
    /// a reconcile can rebuild, and would silently destroy a user action, which
    /// nothing can. So a group that has never acked — including one that has
    /// just been added and holds no row yet — pins the log rather than being
    /// stepped over.
    pub async fn trim(&self, stream: &str, groups: &[&str]) -> Result<u64, EventLogError> {
        if groups.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db.begin_write().await.map_err(backend)?;

        let mut floor = i64::MAX;
        for group in groups {
            let sql = self.db.q("SELECT acked_id FROM event_log_offsets \
                 WHERE project_id = ? AND stream = ? AND consumer_group = ?");
            let acked: Option<i64> = sqlx::query_scalar(&sql)
                .bind(self.project.as_str())
                .bind(stream)
                .bind(*group)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
            // No row means this group has consumed nothing. Trimming past it
            // would hand it a stream that begins after events it never saw.
            let Some(acked) = acked else { return Ok(0) };
            floor = floor.min(acked);
        }

        let deleted = sqlx::query(
            &self
                .db
                .q("DELETE FROM event_log WHERE project_id = ? AND stream = ? AND id <= ?"),
        )
        .bind(self.project.as_str())
        .bind(stream)
        .bind(floor)
        .execute(&mut *tx)
        .await
        .map_err(backend)?
        .rows_affected();

        tx.commit().await.map_err(backend)?;
        Ok(deleted)
    }
}

#[async_trait::async_trait]
impl EventLog for DbEventLog {
    async fn append(&self, stream: &str, events: &[ProjectEvent]) -> Result<(), EventLogError> {
        if events.is_empty() {
            return Ok(());
        }
        let now = crate::user_inbox::now_ms_i64();
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        for event in events {
            let payload =
                serde_json::to_vec(event).map_err(|e| EventLogError::Encode(e.to_string()))?;
            // `DO NOTHING` is the whole dedup-on-production story: a producer
            // that crashed between appending and recording that it had appended
            // re-derives the same id and writes nothing the second time.
            sqlx::query(&self.db.q(
                "INSERT INTO event_log (project_id, stream, event_id, payload, created_at) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT (project_id, stream, event_id) DO NOTHING",
            ))
            .bind(self.project.as_str())
            .bind(stream)
            .bind(event.id())
            .bind(payload)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn read(
        &self,
        stream: &str,
        group: &str,
        limit: i64,
    ) -> Result<Vec<Delivered>, EventLogError> {
        let sql = self.db.q("SELECT acked_id FROM event_log_offsets \
             WHERE project_id = ? AND stream = ? AND consumer_group = ?");
        let cursor: i64 = sqlx::query_scalar(&sql)
            .bind(self.project.as_str())
            .bind(stream)
            .bind(group)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?
            .unwrap_or(0);

        // The dialects differ by one clause, and that clause is the entire
        // reason this log is safe on PostgreSQL. `BIGSERIAL` is allocated
        // before commit, so a reader trusting `id` alone can record 13 and
        // permanently skip a 12 that commits a moment later. Gating on the
        // transaction id — monotonic with respect to commit — reads only rows
        // nothing in flight can still land beneath.
        //
        // SQLite needs no such clause and must not have one: it has a single
        // writer taking its lock up front, so insert order is commit order.
        let visible = match self.db.dialect() {
            Dialect::Sqlite => "",
            Dialect::Postgres => " AND xid < pg_snapshot_xmin(pg_current_snapshot())",
        };
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT id, payload FROM event_log \
             WHERE project_id = ? AND stream = ? AND id > ?{visible} \
             ORDER BY id LIMIT ?"
        )))
        .bind(self.project.as_str())
        .bind(stream)
        .bind(cursor)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await
        .map_err(backend)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.try_get("id").map_err(backend)?;
            let payload: Vec<u8> = row.try_get("payload").map_err(backend)?;
            match serde_json::from_slice(&payload) {
                Ok(event) => out.push(Delivered { id, event }),
                // Skipped, never fatal. A payload this build cannot decode —
                // an event kind added by a newer node, say — must not end the
                // stream for every consumer behind it. Skipping keeps the blast
                // radius at one event, and the ack below still moves past it.
                Err(e) => {
                    tracing::warn!(error = %e, id, stream, "event did not decode; skipping it");
                }
            }
        }
        Ok(out)
    }

    async fn ack(&self, stream: &str, group: &str, up_to: i64) -> Result<(), EventLogError> {
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        // `acked_id < excluded.acked_id` keeps a cursor from going backwards. A
        // late ack from a consumer that was overtaken would otherwise replay
        // everything between, and at-least-once turns into at-least-many.
        sqlx::query(&self.db.q("INSERT INTO event_log_offsets \
             (project_id, stream, consumer_group, acked_id, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (project_id, stream, consumer_group) DO UPDATE SET \
             acked_id = excluded.acked_id, updated_at = excluded.updated_at \
             WHERE event_log_offsets.acked_id < excluded.acked_id"))
        .bind(self.project.as_str())
        .bind(stream)
        .bind(group)
        .bind(up_to)
        .bind(crate::user_inbox::now_ms_i64())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    const S: &str = "project";

    async fn log() -> DbEventLog {
        DbEventLog::new(
            crate::db::testing::db().await,
            crate::projects::ProjectId::new("1"),
        )
    }

    fn observed(agent: &str, status: &str) -> ProjectEvent {
        ProjectEvent::AgentRunObserved {
            session_id: "s1".into(),
            agent_id: agent.into(),
            preset: None,
            status: status.into(),
            started_at: 1_000,
            ended_at: None,
        }
    }

    async fn read_ids(log: &DbEventLog, group: &str) -> Vec<i64> {
        log.read(S, group, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect()
    }

    /// The ids a group can see, once it can see `want` of them.
    ///
    /// Polling rather than reading once, because on PostgreSQL delivery is
    /// *eventual* by design. The visibility gate holds a row back until no
    /// transaction older than it is still running, and transaction ids are
    /// cluster-global — so an unrelated write anywhere on the server, including
    /// another test in this very run, legitimately delays a committed row.
    /// Asserting on a single read would make every test here a race.
    async fn ids_when(log: &DbEventLog, group: &str, want: usize) -> Vec<i64> {
        for _ in 0..100 {
            let got = read_ids(log, group).await;
            if got.len() >= want {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("group '{group}' never saw {want} event(s)");
    }

    #[tokio::test]
    async fn a_consumer_reads_what_was_appended_in_order() {
        let log = log().await;
        log.append(S, &[observed("a", "running"), observed("b", "running")])
            .await
            .unwrap();

        let ids = ids_when(&log, "g", 2).await;
        assert!(ids[0] < ids[1], "delivery must be in append order");
        let got = log.read(S, "g", 100).await.unwrap();
        assert_eq!(got[0].event, observed("a", "running"));
    }

    /// The at-least-once guarantee itself. An unacked event has to come back,
    /// or a consumer that crashed mid-batch loses it silently.
    #[tokio::test]
    async fn an_unacked_event_is_delivered_again() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        assert_eq!(ids_when(&log, "g", 1).await.len(), 1);
        assert_eq!(
            read_ids(&log, "g").await.len(),
            1,
            "reading without acking must not consume"
        );
    }

    #[tokio::test]
    async fn acking_advances_past_what_was_read() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        let first = ids_when(&log, "g", 1).await;
        log.ack(S, "g", first[0]).await.unwrap();
        assert!(read_ids(&log, "g").await.is_empty());
    }

    /// Two groups are two independent cursors — the property that makes this a
    /// log rather than a queue. One consumer acking must not consume another's
    /// copy, or adding a second subscriber would starve the first.
    #[tokio::test]
    async fn one_groups_ack_does_not_move_another_groups_cursor() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        let mine = ids_when(&log, "runs", 1).await;
        log.ack(S, "runs", mine[0]).await.unwrap();

        assert!(read_ids(&log, "runs").await.is_empty());
        assert_eq!(
            ids_when(&log, "inbox", 1).await.len(),
            1,
            "the other group has acked nothing and must still see it"
        );
    }

    /// Dedup on production. A producer that crashed between appending and
    /// recording that it appended re-derives the same id, and must write
    /// nothing the second time.
    #[tokio::test]
    async fn appending_the_same_event_twice_stores_it_once() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        log.append(S, &[observed("a", "running")]).await.unwrap();
        assert_eq!(ids_when(&log, "g", 1).await.len(), 1);
    }

    /// And the other half: a run whose state moved is a different event, or the
    /// index would never learn it finished.
    #[tokio::test]
    async fn an_event_whose_state_changed_is_appended_separately() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        log.append(S, &[observed("a", "failed")]).await.unwrap();
        assert_eq!(ids_when(&log, "g", 2).await.len(), 2);
    }

    /// A cursor must never go backwards. A late ack from a consumer that was
    /// overtaken would otherwise replay everything in between, turning
    /// at-least-once into at-least-many.
    #[tokio::test]
    async fn a_late_lower_ack_does_not_rewind_the_cursor() {
        let log = log().await;
        log.append(S, &[observed("a", "running"), observed("b", "running")])
            .await
            .unwrap();
        let all = ids_when(&log, "g", 2).await;
        log.ack(S, "g", all[1]).await.unwrap();
        log.ack(S, "g", all[0]).await.unwrap();
        assert!(
            read_ids(&log, "g").await.is_empty(),
            "the earlier ack must not resurrect an acked event"
        );
    }

    /// Two streams never bleed into each other.
    #[tokio::test]
    async fn a_consumer_hears_nothing_from_another_stream() {
        let log = log().await;
        log.append("other", &[observed("a", "running")])
            .await
            .unwrap();
        assert!(read_ids(&log, "g").await.is_empty());
    }

    #[tokio::test]
    async fn trimming_drops_what_every_group_has_acked() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        let all = ids_when(&log, "runs", 1).await;
        log.ack(S, "runs", all[0]).await.unwrap();
        log.ack(S, "inbox", all[0]).await.unwrap();

        assert_eq!(log.trim(S, &["runs", "inbox"]).await.unwrap(), 1);
    }

    /// The rule that keeps a source-of-truth event from being destroyed: a
    /// group that has consumed nothing pins the log. Without this, adding a
    /// subscriber would hand it a stream starting after everything it missed.
    #[tokio::test]
    async fn a_group_that_never_acked_pins_the_log() {
        let log = log().await;
        log.append(S, &[observed("a", "running")]).await.unwrap();
        let all = ids_when(&log, "runs", 1).await;
        log.ack(S, "runs", all[0]).await.unwrap();

        assert_eq!(
            log.trim(S, &["runs", "inbox"]).await.unwrap(),
            0,
            "'inbox' holds no offset row, so nothing may be trimmed"
        );
        assert_eq!(ids_when(&log, "inbox", 1).await.len(), 1);
    }

    /// Trimming stops at the slowest group, not the fastest.
    #[tokio::test]
    async fn trimming_stops_at_the_slowest_group() {
        let log = log().await;
        log.append(S, &[observed("a", "running"), observed("b", "running")])
            .await
            .unwrap();
        let all = ids_when(&log, "runs", 2).await;
        log.ack(S, "runs", all[1]).await.unwrap();
        log.ack(S, "inbox", all[0]).await.unwrap();

        assert_eq!(log.trim(S, &["runs", "inbox"]).await.unwrap(), 1);
        assert_eq!(ids_when(&log, "inbox", 1).await, vec![all[1]]);
    }

    /// The cost of the PostgreSQL visibility gate, asserted rather than left
    /// to be discovered.
    ///
    /// A committed event is held back while any *older* write transaction is
    /// still running, because until that one lands it could still commit a row
    /// beneath this reader's cursor — the skip the gate exists to prevent.
    /// Transaction ids are cluster-wide, so the delaying write need not touch
    /// this stream, this table, or even this database.
    ///
    /// This is why delivery on PostgreSQL is *eventual*, and why every other
    /// test here polls. It is also why the design depends on this server's write
    /// transactions being short: the log's latency is the lifetime of the oldest
    /// write transaction anywhere on the server.
    #[tokio::test]
    async fn an_older_open_write_transaction_delays_delivery() {
        let Some(db) = crate::db::testing::postgres().await else {
            eprintln!("skipped: HORSIE_TEST_POSTGRES_URL is not set");
            return;
        };
        let log = DbEventLog::new(db.clone(), crate::projects::ProjectId::new("1"));

        // A write transaction that has taken an id and is still running. It has
        // to actually write, or PostgreSQL never assigns it one and it holds
        // nothing back.
        let mut holder = db.begin_write().await.unwrap();
        sqlx::query(&db.q(
            "INSERT INTO event_log (project_id, stream, event_id, payload, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        ))
        .bind("1")
        .bind("holder")
        .bind("holder-1")
        .bind(b"{}".to_vec())
        .bind(0_i64)
        .execute(&mut *holder)
        .await
        .unwrap();

        log.append(S, &[observed("a", "running")]).await.unwrap();
        assert!(
            read_ids(&log, "g").await.is_empty(),
            "a row committed after an older open write transaction must not be delivered yet"
        );

        holder.commit().await.unwrap();
        assert_eq!(
            ids_when(&log, "g", 1).await.len(),
            1,
            "once nothing older is running, the event must arrive"
        );
    }

    #[tokio::test]
    async fn appending_nothing_is_not_an_error() {
        log().await.append(S, &[]).await.unwrap();
    }
}
