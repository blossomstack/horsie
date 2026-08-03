//! SQLite-backed [`Journal`].
//!
//! Lives in this crate, not `horsie-actor`, because it shares the settings
//! database: one database takes one sqlx migrator, so the schema belongs to the
//! server's migration chain (`server/migrations/0017_journal.sql`), and keeping
//! the DDL and the queries together beats splitting them across a crate.
//!
//! The property that matters is that **sequence numbers are stored, not
//! counted**. `journal_logs.last_seq` is the allocator; deleting events cannot
//! renumber the survivors, which is what makes compaction safe and what the
//! `Journal` trait already promises.

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use horsie_actor::{Journal, JournalError, JournalResult, PersistenceId};
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};

/// A [`Journal`] over the server's SQLite pool.
pub struct SqliteJournal {
    pool: SqlitePool,
}

impl SqliteJournal {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// The `log_id` for `pid`, or `None` when this actor has never persisted.
    /// Reads go through this so they never create a row as a side effect.
    async fn log_id(&self, pid: &PersistenceId) -> JournalResult<Option<i64>> {
        sqlx::query_scalar("SELECT log_id FROM journal_logs WHERE kind = ? AND id = ?")
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)
    }

    /// The `log_id` for `pid`, creating the row if absent. Only writes call this.
    async fn log_id_for_write(
        tx: &mut Transaction<'_, Sqlite>,
        pid: &PersistenceId,
    ) -> JournalResult<i64> {
        // `DO NOTHING` then `SELECT` rather than `RETURNING`: on the conflict
        // path there is no returned row, and the select is an index hit anyway.
        sqlx::query("INSERT INTO journal_logs (kind, id) VALUES (?, ?) ON CONFLICT DO NOTHING")
            .bind(&pid.kind)
            .bind(&pid.id)
            .execute(&mut **tx)
            .await
            .map_err(backend)?;
        sqlx::query_scalar("SELECT log_id FROM journal_logs WHERE kind = ? AND id = ?")
            .bind(&pid.kind)
            .bind(&pid.id)
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)
    }
}

fn backend(e: sqlx::Error) -> JournalError {
    JournalError::Backend(e.to_string())
}

/// SQLite has no unsigned integers, so sequence numbers cross the boundary as
/// `i64`. A journal would need ~9.2 quintillion events to overflow; saturating
/// is still better than a panic in a durability path.
fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

#[async_trait]
impl Journal for SqliteJournal {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        if events.is_empty() {
            return Ok(());
        }
        // IMMEDIATE takes the write lock up front. Without it the transaction
        // starts deferred and upgrades on the first write, which is where two
        // concurrent writers deadlock instead of one simply waiting.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let log_id = Self::log_id_for_write(&mut tx, pid).await?;

        // Allocate the whole batch's numbers in one update, then read the base.
        // The batch is one transaction, so a crash mid-write leaves neither the
        // numbers nor the rows — the actor advances `seq_nr` only after `persist`
        // returns `Ok`, so a half-written batch must not be half-applied.
        let last_seq: i64 = sqlx::query_scalar(
            "UPDATE journal_logs SET last_seq = last_seq + ? WHERE log_id = ? RETURNING last_seq",
        )
        .bind(to_i64(events.len() as u64))
        .bind(log_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        let base = last_seq - events.len() as i64;

        for (offset, payload) in events.iter().enumerate() {
            sqlx::query("INSERT INTO journal_events (log_id, seq, payload) VALUES (?, ?, ?)")
                .bind(log_id)
                .bind(base + offset as i64 + 1)
                .bind(payload.as_slice())
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<(u64, Vec<u8>)>> {
        // An index range scan from the cursor: the cost is the tail returned,
        // not the length of the log. Collected rather than streamed because the
        // borrow would otherwise outlive this `async fn`'s scope; a recovery
        // reads only what a snapshot did not already cover.
        let log_id = match self.log_id(pid).await {
            Ok(Some(id)) => id,
            Ok(None) => return stream::empty().boxed(),
            Err(e) => return stream::iter(vec![Err(e)]).boxed(),
        };
        let rows = sqlx::query(
            "SELECT seq, payload FROM journal_events \
             WHERE log_id = ? AND seq > ? ORDER BY seq",
        )
        .bind(log_id)
        .bind(to_i64(after_seq))
        .fetch_all(&self.pool)
        .await;

        match rows {
            Ok(rows) => stream::iter(
                rows.into_iter()
                    .map(|r| {
                        Ok((
                            to_u64(r.get::<i64, _>("seq")),
                            r.get::<Vec<u8>, _>("payload"),
                        ))
                    })
                    .collect::<Vec<_>>(),
            )
            .boxed(),
            Err(e) => stream::iter(vec![Err(backend(e))]).boxed(),
        }
    }

    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let log_id = Self::log_id_for_write(&mut tx, pid).await?;
        // A snapshot may be taken at a sequence this log has not reached when
        // the state came from elsewhere; keep `last_seq` monotonic so later
        // events never reuse a number the snapshot already covers.
        sqlx::query("UPDATE journal_logs SET last_seq = MAX(last_seq, ?) WHERE log_id = ?")
            .bind(to_i64(seq_nr))
            .bind(log_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(
            "INSERT INTO journal_snapshots (log_id, seq, state) VALUES (?, ?, ?) \
             ON CONFLICT(log_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        )
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
        let row = sqlx::query("SELECT state, seq FROM journal_snapshots WHERE log_id = ?")
            .bind(log_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        Ok(row.map(|r| (r.get::<Vec<u8>, _>("state"), to_u64(r.get::<i64, _>("seq")))))
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        let Some(log_id) = self.log_id(pid).await? else {
            return Ok(());
        };
        // `last_seq` is untouched, so the survivors keep their numbers and the
        // next event continues from where the log actually is.
        sqlx::query("DELETE FROM journal_events WHERE log_id = ? AND seq <= ?")
            .bind(log_id)
            .bind(to_i64(seq_nr))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let src = sqlx::query(
            "SELECT s.state, s.seq FROM journal_snapshots s \
             JOIN journal_logs l ON l.log_id = s.log_id \
             WHERE l.kind = ? AND l.id = ?",
        )
        .bind(&from.kind)
        .bind(&from.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?
        // Erroring beats succeeding emptily: the caller forks a session from
        // this snapshot, and a silent miss produces an agent with no history.
        .ok_or_else(|| JournalError::Backend(format!("no snapshot for '{from}'")))?;

        let state: Vec<u8> = src.get("state");
        let seq: i64 = src.get("seq");
        let dst = Self::log_id_for_write(&mut tx, to).await?;
        // The destination starts with an empty event log at the source's
        // snapshot sequence, so a fresh actor recovers the copied state and
        // numbers its own first event from there.
        sqlx::query("UPDATE journal_logs SET last_seq = ? WHERE log_id = ?")
            .bind(seq)
            .bind(dst)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query("DELETE FROM journal_events WHERE log_id = ?")
            .bind(dst)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(
            "INSERT INTO journal_snapshots (log_id, seq, state) VALUES (?, ?, ?) \
             ON CONFLICT(log_id) DO UPDATE SET seq = excluded.seq, state = excluded.state",
        )
        .bind(dst)
        .bind(seq)
        .bind(state.as_slice())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        // The cascade takes the events and the snapshot with it.
        sqlx::query("DELETE FROM journal_logs WHERE kind = ? AND id = ?")
            .bind(&pid.kind)
            .bind(&pid.id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
    // This module tests a `Journal` implementation, so reading one directly is
    // the subject rather than a violation. Everywhere else the ban stands: only
    // an actor reads its own journal, and only to recover.
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use horsie_actor::testkit::conformance;

    async fn journal(dir: &tempfile::TempDir) -> SqliteJournal {
        let url = format!("sqlite://{}/journal.db", dir.path().display());
        SqliteJournal::new(crate::config::store::open_pool(&url).await.unwrap())
    }

    fn pid(id: &str) -> PersistenceId {
        PersistenceId::new("t", id)
    }

    async fn drain(j: &SqliteJournal, id: &str, after: u64) -> Vec<(u64, Vec<u8>)> {
        let mut s = j.replay(&pid(id), after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    // ── the contract, shared with every other backend ────────────────────────

    macro_rules! conformance_tests {
        ($($name:ident),* $(,)?) => {$(
            #[tokio::test]
            async fn $name() {
                let d = tempfile::tempdir().unwrap();
                conformance::$name(&journal(&d).await).await;
            }
        )*};
    }

    conformance_tests!(
        persist_then_replay_returns_events_in_order,
        replay_skips_events_at_or_before_after_seq,
        logs_are_namespaced_by_kind,
        clear_removes_all_state,
        persist_continues_numbering_after_compaction,
        snapshot_roundtrips_with_seq,
        delete_events_before_compacts,
        copy_snapshot_seeds_new_id,
        copy_snapshot_without_source_errors,
        snapshot_then_compact_leaves_only_later_events,
    );

    // ── backend specifics ────────────────────────────────────────────────────

    /// The property the whole design turns on: numbers come from `last_seq`, so
    /// compaction cannot renumber what survives and a cursor stays meaningful.
    #[tokio::test]
    async fn compaction_never_renumbers_the_survivors() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        j.persist(&pid("n"), &[vec![1], vec![2], vec![3], vec![4]])
            .await
            .unwrap();
        assert_eq!(
            drain(&j, "n", 0)
                .await
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        j.delete_events_before(&pid("n"), 2).await.unwrap();
        assert_eq!(
            drain(&j, "n", 0)
                .await
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "survivors keep their original numbers"
        );

        j.persist(&pid("n"), &[vec![5]]).await.unwrap();
        assert_eq!(
            drain(&j, "n", 4).await,
            vec![(5, vec![5])],
            "the next event continues from last_seq, not from MAX(seq)"
        );
    }

    /// A replay from a cursor must yield the tail, and nothing before it — the
    /// read the file backend could only answer by decoding the whole log.
    #[tokio::test]
    async fn replay_from_a_cursor_yields_only_the_tail() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        let events: Vec<Vec<u8>> = (0u8..50).map(|i| vec![i]).collect();
        j.persist(&pid("long"), &events).await.unwrap();

        let tail = drain(&j, "long", 47).await;
        assert_eq!(
            tail,
            vec![(48, vec![47]), (49, vec![48]), (50, vec![49])],
            "only the events after the cursor, with their own numbers"
        );
    }

    #[tokio::test]
    async fn a_batch_is_numbered_contiguously_from_the_log_head() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        j.persist(&pid("b"), &[vec![1]]).await.unwrap();
        j.persist(&pid("b"), &[vec![2], vec![3], vec![4]])
            .await
            .unwrap();
        assert_eq!(
            drain(&j, "b", 0).await,
            vec![(1, vec![1]), (2, vec![2]), (3, vec![3]), (4, vec![4])]
        );
    }

    #[tokio::test]
    async fn a_fork_continues_numbering_from_the_copied_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        j.persist(&pid("src"), &[vec![1], vec![2]]).await.unwrap();
        j.save_snapshot(&pid("src"), vec![9], 2).await.unwrap();
        j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();

        j.persist(&pid("dst"), &[vec![3]]).await.unwrap();
        assert_eq!(
            drain(&j, "dst", 2).await,
            vec![(3, vec![3])],
            "the fork's first event follows the snapshot it was seeded with"
        );
        // And the source is untouched by the copy.
        assert_eq!(drain(&j, "src", 0).await.len(), 2);
    }

    #[tokio::test]
    async fn reading_an_unknown_log_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        assert!(drain(&j, "ghost", 0).await.is_empty());
        assert_eq!(j.latest_snapshot(&pid("ghost")).await.unwrap(), None);
        // A read must not have inserted a log row as a side effect.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal_logs")
            .fetch_one(&j.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn payloads_survive_arbitrary_bytes() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        let payload = vec![0u8, b'\n', 255, b'"', 0x1f];
        j.persist(&pid("bin"), std::slice::from_ref(&payload))
            .await
            .unwrap();
        assert_eq!(drain(&j, "bin", 0).await, vec![(1, payload)]);
    }

    /// The payoff: after a snapshot compacts the log, a fresh actor recovers the
    /// same state while replaying only what came after it. This is what turns an
    /// O(transcript) recovery into an O(events-since-snapshot) one.
    #[tokio::test]
    async fn recovery_reads_the_snapshot_and_only_the_events_after_it() {
        use futures_util::StreamExt;
        use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, spawn_root};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Default, Clone)]
        #[serde(default)]
        struct SumState {
            total: i64,
        }
        #[derive(Serialize, Deserialize, Clone)]
        struct Added(i64);
        enum Cmd {
            Add(i64),
            Snapshot,
            Get(tokio::sync::oneshot::Sender<i64>),
        }
        struct Sum;

        #[async_trait]
        impl EventSourcedActor for Sum {
            type Command = Cmd;
            type Event = Added;
            type State = SumState;
            fn persistence_id(&self) -> PersistenceId {
                PersistenceId::new("sum", "s1")
            }
            fn initial_state() -> SumState {
                SumState::default()
            }
            fn apply_event(mut s: SumState, e: Added) -> SumState {
                s.total += e.0;
                s
            }
            async fn handle_command(
                &mut self,
                state: &SumState,
                cmd: Cmd,
                _ctx: &mut ActorContext<Self>,
            ) -> CommandEffect<Added> {
                match cmd {
                    Cmd::Add(n) => CommandEffect::persist(vec![Added(n)]),
                    Cmd::Snapshot => CommandEffect::snapshot(),
                    Cmd::Get(tx) => {
                        let _ = tx.send(state.total);
                        CommandEffect::none()
                    }
                }
            }
        }

        let d = tempfile::tempdir().unwrap();
        let j = std::sync::Arc::new(journal(&d).await);
        let pid = PersistenceId::new("sum", "s1");

        // Four events, a snapshot (which compacts), then one more.
        let a = spawn_root(Sum, j.clone() as std::sync::Arc<dyn Journal>);
        for n in [1, 2, 3, 4] {
            a.tell(Cmd::Add(n)).await.unwrap();
        }
        a.tell(Cmd::Snapshot).await.unwrap();
        a.tell(Cmd::Add(5)).await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        a.tell(Cmd::Get(tx)).await.unwrap();
        assert_eq!(rx.await.unwrap(), 15);

        // The snapshot was taken at event 4 and compacted everything up to it.
        assert_eq!(
            j.latest_snapshot(&pid).await.unwrap().map(|(_, s)| s),
            Some(4)
        );
        let mut remaining = j.replay(&pid, 0).await;
        let mut left = Vec::new();
        while let Some(item) = remaining.next().await {
            left.push(item.unwrap().0);
        }
        assert_eq!(
            left,
            vec![5],
            "only the post-snapshot event survives, keeping its own number"
        );

        // A second incarnation recovers the same total from snapshot + tail.
        let b = spawn_root(Sum, j.clone() as std::sync::Arc<dyn Journal>);
        let (tx, rx) = tokio::sync::oneshot::channel();
        b.tell(Cmd::Get(tx)).await.unwrap();
        assert_eq!(
            rx.await.unwrap(),
            15,
            "snapshot (10) plus the one replayed event (5)"
        );
    }

    #[tokio::test]
    async fn clear_removes_events_and_snapshot_together() {
        let d = tempfile::tempdir().unwrap();
        let j = journal(&d).await;
        j.persist(&pid("c"), &[vec![1]]).await.unwrap();
        j.save_snapshot(&pid("c"), vec![7], 1).await.unwrap();
        j.clear(&pid("c")).await.unwrap();
        assert!(drain(&j, "c", 0).await.is_empty());
        assert_eq!(j.latest_snapshot(&pid("c")).await.unwrap(), None);
        // Numbering restarts, because the log itself is gone.
        j.persist(&pid("c"), &[vec![2]]).await.unwrap();
        assert_eq!(drain(&j, "c", 0).await, vec![(1, vec![2])]);
    }
}
