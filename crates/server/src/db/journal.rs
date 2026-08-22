//! [`Journal`] on the server's database, for either backend.
//!
//! Lives in this crate, not `horsie-actor`, because it shares the settings
//! database: one database takes one sqlx migrator, so the schema belongs to
//! the server's migration chain (`server/migrations/*/0017_journal.sql`), and
//! keeping the DDL and the queries together beats splitting them across a
//! crate.
//!
//! The property that matters is that **sequence numbers are stored, not
//! counted**. `journal_logs.last_seq` is the allocator; deleting events cannot
//! renumber the survivors, which is what makes compaction safe and what the
//! `Journal` trait already promises. Keeping the allocator in the database
//! rather than in a per-process cache is also what lets the numbers stay
//! correct without this type having to assume it is the only writer.
//!
//! This is the only journal the server has. The `FileJournal` that preceded it
//! no-oped every snapshot method — fine for a short single-shot run that always
//! full-replays, wrong for a server that offloads idle actors and re-recovers
//! them on demand — and left with the `journal.backend` setting that selected
//! it.

use crate::db::Db;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use horsie_actor::{Journal, JournalError, JournalResult, PersistenceId};
use sqlx::{Any, Row, Transaction};
use std::collections::VecDeque;

/// Events fetched per `replay` page. Keyset pagination keeps memory flat on a
/// log that has not been compacted recently, without holding a cursor (or a
/// borrowed query string) open across the whole stream.
const REPLAY_PAGE_ROWS: i64 = 1_000;

/// Rows per `INSERT` statement in `persist`.
///
/// Both backends cap bind parameters per statement — PostgreSQL at 65 535,
/// SQLite at 32 766 — and each row binds three. 1 000 rows is 3 000 parameters,
/// comfortably inside both. Real batches are single digits, so the chunking is
/// not the point: it exists so a pathological batch degrades into several
/// statements instead of one error.
const INSERT_CHUNK_ROWS: usize = 1_000;

/// `INSERT INTO journal_events … VALUES (?, ?, ?), (?, ?, ?), …` for `rows`
/// rows, in SQLite's placeholder style for [`Db::q`] to translate.
///
/// One statement per chunk rather than one per event: inside a transaction each
/// `execute` is still a round trip, and on PostgreSQL that is a network hop.
/// A 50-event turn goes from 50 of them to one.
pub(crate) fn insert_statement(rows: usize) -> String {
    let mut sql = String::with_capacity(64 + rows * 11);
    sql.push_str("INSERT INTO journal_events (log_id, seq, payload) VALUES ");
    for i in 0..rows {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?)");
    }
    sql
}

/// A [`Journal`] over the server's database, on either dialect.
///
/// **One per node, not one per account.** A log is found by `(kind, id)`
/// alone, which every persistence id here already satisfies uniquely — an
/// account id is random, a session and an agent are uuids. It could not be
/// otherwise: a [`Journal`] method receives a [`PersistenceId`] and nothing
/// else, and a persistence id is fixed when its actor is constructed, which
/// for a shard actor is before a single byte of its history has been read. So
/// an account could only reach the journal by being packed into the id — and a
/// journal is a framework thing, where not every user of it has accounts at
/// all.
///
/// What the `user_id` column used to buy was that another account's log was
/// unreachable even given its id. That protection is now that the id is a uuid
/// nothing hands out. Deleting an account's data walks its supervisor's session
/// list rather than filtering a column.
pub struct SqlJournal {
    db: Db,
}

impl SqlJournal {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The `log_id` for `pid`, or `None` when this actor has never persisted.
    /// Reads go through this so they never create a row as a side effect.
    async fn log_id(&self, pid: &PersistenceId) -> JournalResult<Option<i64>> {
        let sql = self
            .db
            .q("SELECT log_id FROM journal_logs WHERE kind = ? AND id = ?");
        sqlx::query_scalar(&sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)
    }

    /// The `log_id` for `pid`, creating the row if absent. Only writes call
    /// this.
    async fn log_id_for_write(
        db: &Db,
        tx: &mut Transaction<'_, Any>,
        pid: &PersistenceId,
    ) -> JournalResult<i64> {
        // `DO NOTHING` then `SELECT` rather than `RETURNING`: on the conflict
        // path there is no returned row, and the select is an index hit anyway.
        let insert =
            db.q("INSERT INTO journal_logs (kind, id) VALUES (?, ?) ON CONFLICT DO NOTHING");
        sqlx::query(&insert)
            .bind(&pid.kind)
            .bind(&pid.id)
            .execute(&mut **tx)
            .await
            .map_err(backend)?;
        let select = db.q("SELECT log_id FROM journal_logs WHERE kind = ? AND id = ?");
        sqlx::query_scalar(&select)
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)
    }

    /// Reject a write whose picture of the log is out of date, inside the
    /// caller's transaction.
    ///
    /// Written as an `UPDATE` that sets `last_seq` to the value it already has,
    /// which looks pointless and is not: a `SELECT` followed by a write is two
    /// statements with a gap between them, and under PostgreSQL's default
    /// isolation another writer can slip into that gap. An update that filters
    /// on the value it is checking takes the row lock and does the check in one
    /// indivisible step.
    ///
    /// `persist` does not call this — it folds the same condition into the
    /// update it was already making, which is cheaper still. This is for
    /// writers with no such update to fold into.
    async fn admit(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        log_id: i64,
        expected_last_seq: u64,
    ) -> JournalResult<()> {
        let sql = self.db.q("UPDATE journal_logs SET last_seq = ? \
             WHERE log_id = ? AND last_seq = ?");
        let affected = sqlx::query(&sql)
            .bind(to_i64(expected_last_seq))
            .bind(log_id)
            .bind(to_i64(expected_last_seq))
            .execute(&mut **tx)
            .await
            .map_err(backend)?
            .rows_affected();
        if affected > 0 {
            return Ok(());
        }
        Err(self.conflict_error(tx, log_id, expected_last_seq).await)
    }

    /// Name both numbers in the rejection rather than just saying no.
    async fn conflict_error(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        log_id: i64,
        expected: u64,
    ) -> JournalError {
        let sql = self
            .db
            .q("SELECT last_seq FROM journal_logs WHERE log_id = ?");
        let actual: Result<i64, _> = sqlx::query_scalar(&sql)
            .bind(log_id)
            .fetch_one(&mut **tx)
            .await;
        match actual {
            Ok(actual) => JournalError::Conflict {
                pid: format!("log {log_id}"),
                expected,
                actual: to_u64(actual),
            },
            Err(e) => backend(e),
        }
    }
}

fn backend(e: sqlx::Error) -> JournalError {
    JournalError::Backend(e.to_string())
}

/// Neither dialect has unsigned integers, so sequence numbers cross the
/// boundary as `i64`. A journal would need ~9.2 quintillion events to
/// overflow; saturating is still better than a panic in a durability path.
fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

#[async_trait]
impl Journal for SqlJournal {
    async fn persist(
        &self,
        pid: &PersistenceId,
        events: &[Vec<u8>],
        expected_last_seq: u64,
    ) -> JournalResult<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        let log_id = Self::log_id_for_write(&self.db, &mut tx, pid).await?;

        // Allocate the whole batch's numbers in one update, then read the base.
        // The batch is one transaction, so a crash mid-write leaves neither the
        // numbers nor the rows — the actor advances `seq_nr` only after
        // `persist` returns `Ok`, so a half-written batch must not be
        // half-applied.
        //
        // The write fence rides along in the same statement rather than taking
        // one of its own. `last_seq = ?` is the whole check: it admits the
        // writer only if the log still ends where that writer believes, which
        // is false the moment anybody else has appended. Folding it into the
        // update that was happening anyway costs nothing and makes
        // check-and-write indivisible by construction rather than by
        // remembering to keep them together.
        let sql = self.db.q("UPDATE journal_logs SET last_seq = last_seq + ? \
             WHERE log_id = ? AND last_seq = ? RETURNING last_seq");
        let admitted: Option<i64> = sqlx::query_scalar(&sql)
            .bind(to_i64(events.len() as u64))
            .bind(log_id)
            .bind(to_i64(expected_last_seq))
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
        // No row means the log has moved past this writer, so nothing was
        // numbered and nothing will be appended.
        let Some(last_seq) = admitted else {
            return Err(self
                .conflict_error(&mut tx, log_id, expected_last_seq)
                .await);
        };
        let base = last_seq - events.len() as i64;

        for (chunk_index, chunk) in events.chunks(INSERT_CHUNK_ROWS).enumerate() {
            let statement = insert_statement(chunk.len());
            let insert = self.db.q(&statement);
            let mut query = sqlx::query(&insert);
            let chunk_base = base + (chunk_index * INSERT_CHUNK_ROWS) as i64;
            for (offset, payload) in chunk.iter().enumerate() {
                query = query
                    .bind(log_id)
                    .bind(chunk_base + offset as i64 + 1)
                    .bind(payload.as_slice());
            }
            query.execute(&mut *tx).await.map_err(backend)?;
        }
        tx.commit().await.map_err(backend)
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<(u64, Vec<u8>)>> {
        // An index range scan from the cursor: the cost is the tail returned,
        // not the length of the log.
        let log_id = match self.log_id(pid).await {
            Ok(Some(id)) => id,
            Ok(None) => return stream::empty().boxed(),
            Err(e) => return stream::iter(vec![Err(e)]).boxed(),
        };
        // Everything the stream needs is owned, so it borrows neither `self`
        // nor the query string — which is what makes keyset pagination simpler
        // here than holding one long-lived sqlx cursor open.
        let init = Page {
            db: self.db.clone(),
            log_id,
            cursor: to_i64(after_seq),
            buffer: VecDeque::new(),
            exhausted: false,
        };
        stream::unfold(init, |mut page| async move {
            loop {
                if let Some(next) = page.buffer.pop_front() {
                    return Some((Ok(next), page));
                }
                if page.exhausted {
                    return None;
                }
                if let Err(e) = page.fetch().await {
                    page.exhausted = true;
                    return Some((Err(e), page));
                }
            }
        })
        .boxed()
    }

    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()> {
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        let log_id = Self::log_id_for_write(&self.db, &mut tx, pid).await?;
        // Conditional like an append, and on the same value: a snapshot claims
        // what the log contains, so a writer whose log has moved on is claiming
        // something false. It is also the more destructive of the two writes —
        // a snapshot is where the next recovery starts, so a stale one rewrites
        // history rather than adding to it.
        self.admit(&mut tx, log_id, seq_nr).await?;
        let upsert = self.db.q(
            "INSERT INTO journal_snapshots (log_id, seq, state) VALUES (?, ?, ?) \
             ON CONFLICT(log_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        );
        sqlx::query(&upsert)
            .bind(log_id)
            .bind(to_i64(seq_nr))
            .bind(state.as_slice())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        let Some(log_id) = self.log_id(pid).await? else {
            return Ok(None);
        };
        let sql = self
            .db
            .q("SELECT state, seq FROM journal_snapshots WHERE log_id = ?");
        let row = sqlx::query(&sql)
            .bind(log_id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?;
        let Some(row) = row else { return Ok(None) };
        let state: Vec<u8> = row.try_get("state").map_err(backend)?;
        let seq: i64 = row.try_get("seq").map_err(backend)?;
        Ok(Some((state, to_u64(seq))))
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        let Some(log_id) = self.log_id(pid).await? else {
            return Ok(());
        };
        // `last_seq` is untouched, so the survivors keep their numbers and the
        // next event continues from where the log actually is.
        let sql = self
            .db
            .q("DELETE FROM journal_events WHERE log_id = ? AND seq <= ?");
        sqlx::query(&sql)
            .bind(log_id)
            .bind(to_i64(seq_nr))
            .execute(self.db.pool())
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        let select = self.db.q("SELECT s.state, s.seq FROM journal_snapshots s \
             JOIN journal_logs l ON l.log_id = s.log_id \
             WHERE l.kind = ? AND l.id = ?");
        let src = sqlx::query(&select)
            .bind(&from.kind)
            .bind(&from.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?
            // Erroring beats succeeding emptily: the caller sub sessions a
            // session from this snapshot, and a silent miss produces an agent
            // with no history.
            .ok_or_else(|| JournalError::Backend(format!("no snapshot for '{from}'")))?;

        let state: Vec<u8> = src.try_get("state").map_err(backend)?;
        let seq: i64 = src.try_get("seq").map_err(backend)?;
        let dst = Self::log_id_for_write(&self.db, &mut tx, to).await?;
        // The destination starts with an empty event log at the source's
        // snapshot sequence, so a fresh actor recovers the copied state and
        // numbers its own first event from there.
        let set_seq = self
            .db
            .q("UPDATE journal_logs SET last_seq = ? WHERE log_id = ?");
        sqlx::query(&set_seq)
            .bind(seq)
            .bind(dst)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let clear_events = self.db.q("DELETE FROM journal_events WHERE log_id = ?");
        sqlx::query(&clear_events)
            .bind(dst)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let upsert = self.db.q(
            "INSERT INTO journal_snapshots (log_id, seq, state) VALUES (?, ?, ?) \
             ON CONFLICT(log_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        );
        sqlx::query(&upsert)
            .bind(dst)
            .bind(seq)
            .bind(state.as_slice())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn last_seq(&self, pid: &PersistenceId) -> JournalResult<u64> {
        let Some(log_id) = self.log_id(pid).await? else {
            // A log nobody has written to ends at 0, which is exactly what its
            // first writer must expect. Creating a row to answer a read would
            // make an observer able to change what it is observing.
            return Ok(0);
        };
        let sql = self
            .db
            .q("SELECT last_seq FROM journal_logs WHERE log_id = ?");
        let last: Option<i64> = sqlx::query_scalar(&sql)
            .bind(log_id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?;
        Ok(last.map_or(0, to_u64))
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        // Deleted explicitly rather than left to `ON DELETE CASCADE`, which
        // fires on PostgreSQL and does nothing on SQLite: `Db::open` turns
        // `PRAGMA foreign_keys` off on every connection, so the declaration in
        // `0017_journal.sql` is documentation and a PostgreSQL backstop, not a
        // mechanism. Relying on it would leave orphaned events and snapshots on
        // SQLite — invisible, since the next `persist` allocates a fresh
        // `log_id`, and so permanent.
        // Resolved before the transaction opens, not inside it from the pool: a
        // second connection taken while holding a write transaction would
        // deadlock outright on a single-connection pool.
        let Some(log_id) = self.log_id(pid).await? else {
            // Nothing to clear, and no row to create by asking.
            return Ok(());
        };
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        for table in ["journal_events", "journal_snapshots"] {
            let statement = format!("DELETE FROM {table} WHERE log_id = ?");
            let sql = self.db.q(&statement);
            sqlx::query(&sql)
                .bind(log_id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        let sql = self.db.q("DELETE FROM journal_logs WHERE log_id = ?");
        sqlx::query(&sql)
            .bind(log_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }
}

/// One `replay` stream's position: the owned state `unfold` threads along.
struct Page {
    db: Db,
    log_id: i64,
    cursor: i64,
    buffer: VecDeque<(u64, Vec<u8>)>,
    exhausted: bool,
}

impl Page {
    /// Fetch the next page, advancing the cursor. A short page means the log is
    /// drained, which is recorded so the stream ends without a final empty
    /// round-trip.
    async fn fetch(&mut self) -> JournalResult<()> {
        let sql = self.db.q("SELECT seq, payload FROM journal_events \
             WHERE log_id = ? AND seq > ? ORDER BY seq LIMIT ?");
        let rows = sqlx::query(&sql)
            .bind(self.log_id)
            .bind(self.cursor)
            .bind(REPLAY_PAGE_ROWS)
            .fetch_all(self.db.pool())
            .await
            .map_err(backend)?;

        if (rows.len() as i64) < REPLAY_PAGE_ROWS {
            self.exhausted = true;
        }
        for row in &rows {
            let seq: i64 = row.try_get("seq").map_err(backend)?;
            let payload: Vec<u8> = row.try_get("payload").map_err(backend)?;
            self.cursor = self.cursor.max(seq);
            self.buffer.push_back((to_u64(seq), payload));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn one_row_is_a_plain_insert() {
        assert_eq!(
            insert_statement(1),
            "INSERT INTO journal_events (log_id, seq, payload) VALUES (?, ?, ?)"
        );
    }

    #[test]
    fn several_rows_share_one_statement() {
        assert!(
            insert_statement(3).ends_with("VALUES (?, ?, ?), (?, ?, ?), (?, ?, ?)"),
            "{}",
            insert_statement(3)
        );
    }

    /// The rows a chunk holds are exactly the rows its statement has slots for.
    /// A mismatch binds the batch off by a row and sqlx reports it as a type
    /// error rather than a miscount, so pin it here.
    #[test]
    fn a_full_chunks_statement_has_one_slot_per_row() {
        let sql = insert_statement(INSERT_CHUNK_ROWS);
        assert_eq!(sql.matches("(?, ?, ?)").count(), INSERT_CHUNK_ROWS);
    }

    /// SQLite-only, like the other migration tests: it builds the schema by
    /// hand and applies exactly 0036, which is the repair for a log 0035
    /// emptied.
    ///
    /// The three rows are the three shapes a log can be in. Only the first is
    /// damaged — a compacted log has no events either, and zeroing *its*
    /// allocator would renumber a history its snapshot still describes.
    #[tokio::test]
    async fn migration_0036_zeroes_the_allocator_of_a_log_with_no_history_left() {
        let pool = &crate::db::testing::unmigrated_sqlite().await;

        for statement in [
            "CREATE TABLE journal_logs (log_id INTEGER PRIMARY KEY, kind TEXT NOT NULL, \
             id TEXT NOT NULL, last_seq INTEGER NOT NULL DEFAULT 0, UNIQUE (kind, id))",
            "CREATE TABLE journal_events (log_id INTEGER NOT NULL, seq INTEGER NOT NULL, \
             payload BLOB NOT NULL, PRIMARY KEY (log_id, seq)) WITHOUT ROWID",
            "CREATE TABLE journal_snapshots (log_id INTEGER PRIMARY KEY, seq INTEGER NOT NULL, \
             state BLOB NOT NULL)",
            "INSERT INTO journal_logs (log_id, kind, id, last_seq) VALUES \
             (1, 'session-supervisor', 'truncated', 82), \
             (2, 'session', 'intact', 3), \
             (3, 'session', 'compacted', 9)",
            "INSERT INTO journal_events (log_id, seq, payload) VALUES (2, 3, x'00')",
            "INSERT INTO journal_snapshots (log_id, seq, state) VALUES (3, 9, x'00')",
        ] {
            sqlx::query(statement).execute(pool).await.unwrap();
        }

        sqlx::query(include_str!(
            "../../migrations/sqlite/0036_journal_repair_truncated.sql"
        ))
        .execute(pool)
        .await
        .unwrap();

        let rows = sqlx::query("SELECT id, last_seq FROM journal_logs ORDER BY log_id")
            .fetch_all(pool)
            .await
            .unwrap();
        let got: Vec<(String, i64)> = rows
            .iter()
            .map(|r| {
                (
                    r.try_get::<String, _>("id").unwrap(),
                    r.try_get::<i64, _>("last_seq").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("truncated".to_string(), 0),
                ("intact".to_string(), 3),
                ("compacted".to_string(), 9),
            ]
        );
    }
}
