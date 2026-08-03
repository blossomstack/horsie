//! [`Journal`] on the server's database, for either backend.
//!
//! The alternative is `FileJournal`, which the CLI still uses. That one no-ops
//! every snapshot method — fine for a short single-shot run that always
//! full-replays, wrong for a server that offloads idle actors and re-recovers
//! them on demand. Here snapshots do what the trait says, so a wake reads one
//! snapshot row plus the events after it instead of the whole log.
//!
//! Sequence numbers are assigned by the journal, not the caller. Asking the
//! database for `MAX(seq)` before every write would double the round-trips on
//! the hot path, so each persistence id keeps its head in memory behind its own
//! async mutex: writes to one actor serialize, writes to different actors do
//! not. The cache is an optimisation over a value the database already holds,
//! and it is only sound because one process owns each persistence id — the
//! composite primary key turns a violation of that into a loud constraint error
//! rather than silent interleaving.

use crate::db::Db;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use horsie_actor::{Journal, JournalError, JournalResult, PersistenceId};
use sqlx::Row;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use tokio::sync::Mutex;

/// Rows per `INSERT` statement.
///
/// Both backends cap bind parameters per statement — PostgreSQL at 65 535,
/// SQLite at 32 766 by default — and each row binds four. 1 000 rows is 4 000
/// parameters, comfortably inside both, and real batches are single digits
/// anyway; the chunking exists so a pathological batch degrades into several
/// statements instead of one error.
const INSERT_CHUNK_ROWS: usize = 1_000;

/// Events fetched per `replay` page. Keyset pagination keeps memory flat on a
/// long log without holding a cursor (or a borrowed query string) open across
/// the whole stream.
const REPLAY_PAGE_ROWS: i64 = 1_000;

/// The in-memory head for one persistence id: the sequence number most recently
/// assigned, or `None` until it has been read back from the database.
type Head = Arc<Mutex<Option<u64>>>;

pub struct SqlJournal {
    db: Db,
    /// Per-persistence-id heads. The outer lock is held only long enough to
    /// clone out an `Arc` — never across an await — so it never serializes
    /// unrelated actors.
    heads: SyncMutex<HashMap<PersistenceId, Head>>,
}

impl SqlJournal {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            heads: SyncMutex::new(HashMap::new()),
        }
    }

    fn head_slot(&self, pid: &PersistenceId) -> Head {
        let mut map = match self.heads.lock() {
            Ok(map) => map,
            // A poisoned lock means a previous holder panicked while updating a
            // head. The map itself is still structurally sound (it holds Arcs,
            // and the value behind each is an async mutex updated elsewhere),
            // so recovering beats propagating a panic into every later write.
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(map.entry(pid.clone()).or_default())
    }

    /// The highest sequence number this log has ever assigned.
    ///
    /// Both terms matter: after compaction the event table can be empty while
    /// the snapshot sits at seq 42, and seeding from events alone would restart
    /// numbering and corrupt the log. Computed in Rust from two scalar reads
    /// rather than in SQL, because the "greatest of two values" function is
    /// spelled differently on the two backends and this keeps one code path.
    async fn read_head(&self, pid: &PersistenceId) -> JournalResult<u64> {
        let events_sql = self
            .db
            .q("SELECT COALESCE(MAX(seq), 0) AS s FROM journal_events WHERE actor_kind = ? AND actor_id = ?");
        let max_event: i64 = sqlx::query(&events_sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_one(self.db.pool())
            .await
            .and_then(|r| r.try_get("s"))
            .map_err(backend)?;

        let snap_sql = self
            .db
            .q("SELECT seq FROM journal_snapshots WHERE actor_kind = ? AND actor_id = ?");
        let max_snapshot: Option<i64> = sqlx::query(&snap_sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?
            .map(|r| r.try_get("seq"))
            .transpose()
            .map_err(backend)?;

        Ok(max_event.max(max_snapshot.unwrap_or(0)).max(0) as u64)
    }

    /// Read the head, seeding the cache from the database on first touch.
    async fn ensure_head(&self, pid: &PersistenceId, slot: &mut Option<u64>) -> JournalResult<u64> {
        match *slot {
            Some(head) => Ok(head),
            None => {
                let head = self.read_head(pid).await?;
                *slot = Some(head);
                Ok(head)
            }
        }
    }
}

#[async_trait]
impl Journal for SqlJournal {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        if events.is_empty() {
            return Ok(());
        }
        let slot = self.head_slot(pid);
        let mut guard = slot.lock().await;
        let head = self.ensure_head(pid, &mut guard).await?;

        let mut tx = self.db.pool().begin().await.map_err(backend)?;
        for (chunk_index, chunk) in events.chunks(INSERT_CHUNK_ROWS).enumerate() {
            let mut sql = String::from(
                "INSERT INTO journal_events (actor_kind, actor_id, seq, payload) VALUES ",
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str("(?, ?, ?, ?)");
            }
            let sql = self.db.q(&sql);
            let mut query = sqlx::query(&sql);
            let base = head + (chunk_index * INSERT_CHUNK_ROWS) as u64;
            for (i, payload) in chunk.iter().enumerate() {
                query = query
                    .bind(&pid.kind)
                    .bind(&pid.id)
                    .bind((base + i as u64 + 1) as i64)
                    .bind(payload.clone());
            }
            query.execute(&mut *tx).await.map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;

        // Only after the commit: a failed write must leave the head where it
        // was, so the retry re-uses the same sequence numbers.
        *guard = Some(head + events.len() as u64);
        Ok(())
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        // Everything the stream needs is owned, so it borrows neither `self` nor
        // the query string — which is what makes keyset pagination simpler here
        // than holding one long-lived sqlx cursor.
        let init = PageState {
            db: self.db.clone(),
            kind: pid.kind.clone(),
            id: pid.id.clone(),
            cursor: after_seq as i64,
            buffer: VecDeque::new(),
            exhausted: false,
        };
        stream::unfold(init, |mut state| async move {
            loop {
                if let Some(next) = state.buffer.pop_front() {
                    return Some((Ok(next), state));
                }
                if state.exhausted {
                    return None;
                }
                match state.fetch_page().await {
                    Ok(()) => {
                        // An empty page after a fetch means the log is drained;
                        // looping once more falls through to `exhausted`.
                        continue;
                    }
                    Err(e) => {
                        state.exhausted = true;
                        return Some((Err(e), state));
                    }
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
        let slot = self.head_slot(pid);
        let mut guard = slot.lock().await;
        let head = self.ensure_head(pid, &mut guard).await?;

        let sql = self.db.q(
            "INSERT INTO journal_snapshots (actor_kind, actor_id, seq, state) VALUES (?, ?, ?, ?) \
             ON CONFLICT (actor_kind, actor_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        );
        sqlx::query(&sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .bind(seq_nr as i64)
            .bind(state)
            .execute(self.db.pool())
            .await
            .map_err(backend)?;

        // Mirrors InMemoryJournal: a snapshot taken at a sequence beyond
        // anything persisted here still advances the log's numbering.
        *guard = Some(head.max(seq_nr));
        Ok(())
    }

    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        let sql = self
            .db
            .q("SELECT state, seq FROM journal_snapshots WHERE actor_kind = ? AND actor_id = ?");
        let row = sqlx::query(&sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(backend)?;
        let Some(row) = row else { return Ok(None) };
        let state: Vec<u8> = row.try_get("state").map_err(backend)?;
        let seq: i64 = row.try_get("seq").map_err(backend)?;
        Ok(Some((state, seq.max(0) as u64)))
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        let sql = self.db.q(
            "DELETE FROM journal_events WHERE actor_kind = ? AND actor_id = ? AND seq <= ?",
        );
        sqlx::query(&sql)
            .bind(&pid.kind)
            .bind(&pid.id)
            .bind(seq_nr as i64)
            .execute(self.db.pool())
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        let (state, seq) = self
            .latest_snapshot(from)
            .await?
            .ok_or_else(|| JournalError::Backend(format!("no snapshot for '{from}'")))?;

        let slot = self.head_slot(to);
        let mut guard = slot.lock().await;

        let mut tx = self.db.pool().begin().await.map_err(backend)?;
        // The target starts with an empty log and the source's snapshot
        // sequence, so a fresh actor recovers the copied state and continues
        // numbering from there.
        let delete = self
            .db
            .q("DELETE FROM journal_events WHERE actor_kind = ? AND actor_id = ?");
        sqlx::query(&delete)
            .bind(&to.kind)
            .bind(&to.id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let upsert = self.db.q(
            "INSERT INTO journal_snapshots (actor_kind, actor_id, seq, state) VALUES (?, ?, ?, ?) \
             ON CONFLICT (actor_kind, actor_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        );
        sqlx::query(&upsert)
            .bind(&to.kind)
            .bind(&to.id)
            .bind(seq as i64)
            .bind(state)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;

        *guard = Some(seq);
        Ok(())
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        let slot = self.head_slot(pid);
        let mut guard = slot.lock().await;

        let mut tx = self.db.pool().begin().await.map_err(backend)?;
        for table in ["journal_events", "journal_snapshots"] {
            let statement = format!("DELETE FROM {table} WHERE actor_kind = ? AND actor_id = ?");
            let sql = self.db.q(&statement);
            sqlx::query(&sql)
                .bind(&pid.kind)
                .bind(&pid.id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;

        *guard = None;
        Ok(())
    }
}

/// One `replay` stream's position: the owned state `unfold` threads along.
struct PageState {
    db: Db,
    kind: String,
    id: String,
    cursor: i64,
    buffer: VecDeque<Vec<u8>>,
    exhausted: bool,
}

impl PageState {
    /// Fetch the next page, advancing the cursor. A short page means the log is
    /// drained, which is recorded so the stream ends without a final empty
    /// round-trip.
    async fn fetch_page(&mut self) -> JournalResult<()> {
        let sql = self.db.q(
            "SELECT seq, payload FROM journal_events \
             WHERE actor_kind = ? AND actor_id = ? AND seq > ? \
             ORDER BY seq LIMIT ?",
        );
        let rows = sqlx::query(&sql)
            .bind(&self.kind)
            .bind(&self.id)
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
            self.buffer.push_back(payload);
        }
        Ok(())
    }
}

fn backend(e: sqlx::Error) -> JournalError {
    JournalError::Backend(e.to_string())
}
